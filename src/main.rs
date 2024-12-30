use anyhow::Context;
use pipewire::{
    loop_::Signal,
    node::{Node, NodeListener, NodeState},
    proxy::{Listener, ProxyListener, ProxyT},
    types::ObjectType,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use zbus::blocking::Connection;
use zbus::zvariant::Value;

const X13S_CAMERA_PRODUCT_NAME: &str = "ov5675";
const X13S_LED_DEVICE_NAME: &str = "white:camera-indicator";
const X13S_LED_BRIGHTNESS_ON: u32 = 1;
const X13S_LED_BRIGHTNESS_OFF: u32 = 0;

struct Nodes {
    nodes_t: HashMap<u32, Node>,
    listeners: HashMap<u32, Vec<Box<dyn Listener>>>,
}

impl Nodes {
    fn new() -> Self {
        Self {
            nodes_t: HashMap::new(),
            listeners: HashMap::new(),
        }
    }
    fn add_node_t(&mut self, node_t: Node, listener: NodeListener) {
        let proxy_id = {
            let proxy = node_t.upcast_ref();
            proxy.id()
        };

        self.nodes_t.insert(proxy_id, node_t);

        let v = self.listeners.entry(proxy_id).or_default();
        v.push(Box::new(listener));
    }
    fn add_proxy_listener(&mut self, proxy_id: u32, listener: ProxyListener) {
        let v = self.listeners.entry(proxy_id).or_default();
        v.push(Box::new(listener));
    }
    fn remove(&mut self, proxy_id: u32) {
        self.nodes_t.remove(&proxy_id);
        self.listeners.remove(&proxy_id);
    }
}

fn monitor() -> anyhow::Result<()> {
    let result = Rc::new(RefCell::new(Ok(())));
    let main_loop = pipewire::main_loop::MainLoop::new(None)?;

    let main_loop_weak = main_loop.downgrade();
    let _sig_int = main_loop.loop_().add_signal_local(Signal::SIGINT, move || {
        if let Some(main_loop) = main_loop_weak.upgrade() {
            main_loop.quit();
        }
    });

    let main_loop_weak = main_loop.downgrade();
    let _sig_term = main_loop
        .loop_()
        .add_signal_local(Signal::SIGTERM, move || {
            if let Some(main_loop) = main_loop_weak.upgrade() {
                main_loop.quit();
            }
        });

    let context = pipewire::context::Context::new(&main_loop)?;
    let core = context.connect(None)?;
    let main_loop_weak = main_loop.downgrade();
    let result_weak = Rc::downgrade(&result);
    let _listener = core
        .add_listener_local()
        .info(|info| {
            log::debug!("{:#?}", info);
        })
        .done(|id, seq| {
            log::debug!("{}, {:?}", id, seq);
        })
        .error(move |id, seq, res, message| {
            log::error!("error id:{} seq:{} res:{}: {}", id, seq, res, message);
            if id == 0 {
                if let Some(main_loop) = main_loop_weak.upgrade() {
                    main_loop.quit();
                    if let Some(result) = result_weak.upgrade() {
                        *result.borrow_mut() = Err(anyhow::anyhow!("pipewire error: {}", message));
                    }
                }
            }
        })
        .register();

    let registry = Rc::new(core.get_registry()?);
    let registry_weak = Rc::downgrade(&registry);

    let nodes = Rc::new(RefCell::new(Nodes::new()));

    let camera_id: Rc<RefCell<Option<u32>>> = Rc::new(RefCell::new(None));

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let camera_id = camera_id.clone();
            move |obj| {
                if let Some(registry) = registry_weak.upgrade() {
                    match obj.type_ {
                        ObjectType::Node => {
                            let camera_id = camera_id.clone();

                            let node: Node = registry.bind(obj).unwrap();
                            let node_listener = node
                                .add_listener_local()
                                .info(move |info| {
                                    if let Some(props) = info.props() {
                                        if props.get("media.role") == Some("Camera")
                                            && props.get("api.libcamera.location") == Some("front")
                                            && props.get("device.product.name")
                                                == Some(X13S_CAMERA_PRODUCT_NAME)
                                        {
                                            log::info!("id:{} is my front camera", info.id());
                                            camera_id.borrow_mut().replace(info.id());
                                        }
                                    }
                                    if *camera_id.borrow() == Some(info.id()) {
                                        log::info!("camera state: {:?}", info.state());
                                        let led_brightness = match info.state() {
                                            NodeState::Running => X13S_LED_BRIGHTNESS_ON,
                                            _ => X13S_LED_BRIGHTNESS_OFF,
                                        };
                                        log::info!("set led brightness: {}", led_brightness);
                                        if let Err(err) = set_led_brightness(led_brightness) {
                                            log::error!("failed to set LED brightness: {:?}", err);
                                            if let Err(err) = notification(
                                                "Camera state changed",
                                                &format!("{:?}", info.state()),
                                            ) {
                                                log::error!(
                                                    "failed to send notification: {:?}",
                                                    err
                                                );
                                            }
                                        }
                                    } else {
                                        // TODO: can I stop listening this camera unrelated one?
                                    }
                                })
                                .register();

                            let proxy = node.upcast_ref();
                            let proxy_id = proxy.id();

                            let nodes_weak = Rc::downgrade(&nodes);

                            let listener = proxy
                                .add_listener_local()
                                .removed(move || {
                                    if let Some(nodes) = nodes_weak.upgrade() {
                                        nodes.borrow_mut().remove(proxy_id);
                                    }
                                })
                                .register();

                            nodes.borrow_mut().add_node_t(node, node_listener);
                            nodes.borrow_mut().add_proxy_listener(proxy_id, listener);
                        }
                        _ => (),
                    }
                }
            }
        })
        .global_remove(move |id| {
            if *camera_id.borrow() == Some(id) {
                log::info!("id:{} my camera removed", id);
                *camera_id.borrow_mut() = None;
            }
        })
        .register();

    main_loop.run();

    Rc::into_inner(result)
        .context("leak `result` reference somewhere")?
        .into_inner()
}

fn set_led_brightness(brightness: u32) -> anyhow::Result<()> {
    static CONNECTION: OnceLock<zbus::Result<Connection>> = OnceLock::new();
    let connection = CONNECTION
        .get_or_init(Connection::system)
        .clone()
        .context("error connecting to system bus")?;
    let _m = connection.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1/session/auto",
        Some("org.freedesktop.login1.Session"),
        "SetBrightness",
        &("leds", X13S_LED_DEVICE_NAME, brightness),
    )?;
    Ok(())
}

fn notification(summary: &str, message: &str) -> anyhow::Result<()> {
    let connection = Connection::session()?;
    let _m = connection.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "org.u7fa9.x13s-camera-led",
            42u32,
            "camera-web-symbolic",
            summary,
            message,
            vec![""; 0],
            HashMap::<&str, &Value>::new(),
            0,
        ),
    )?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    pipewire::init();

    monitor()?;

    Ok(())
}
