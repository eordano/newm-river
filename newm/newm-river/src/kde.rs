//! The KDE session services newm-river serves on the session bus where Plasma has KWin and
//! PowerDevil: `org.freedesktop.ScreenSaver` (nothing owned it, so D-Bus inhibits from X11
//! Chromium, Firefox and VLC were lost and swayidle blanked during video), PowerDevil's
//! `PolicyAgent` (the KDE Inhibit portal's back end), `org.kde.kglobalaccel` (KDE apps' global
//! shortcuts) and KRunner's `/WindowsRunner` (served on the `org.kde.KWin` connection,
//! `crate::screenshot`). The rules live in the `newm-kde` crate; this module puts them on the bus
//! and hands the window manager what it must act on (`crate::river::kde`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use newm_kde::inhibit::{holds_off_idle, Cookies, Hold};
use newm_kde::kglobalaccel::{self as reg, Registry, Seq};
use newm_kde::krunner;
use tracing::{debug, info, warn};
use zbus::message::Header;
use zbus::object_server::{ObjectServer, SignalEmitter};
use zbus::{fdo, interface, OwnedObjectPath, OwnedVariant, Variant};

use crate::screenshot::Scene;

/// What the window manager must act on.
pub enum ToWm {
    /// The keys KDE apps hold changed.
    Bindings(Vec<reg::Binding>),
    /// Whether D-Bus inhibitions hold idle off.
    Inhibit(bool),
    /// KRunner asks for every window.
    Windows(Sender<Vec<krunner::Window>>),
    /// KRunner ran a window's match: focus it.
    Activate(u64),
}

/// Sends to the window manager and wakes its loop.
#[derive(Clone)]
pub struct Wm {
    tx: Sender<ToWm>,
    waker: Arc<UnixStream>,
}

impl Wm {
    pub fn send(&self, msg: ToWm) {
        if self.tx.send(msg).is_ok() {
            let _ = (&*self.waker).write(&[1]);
        }
    }
}

/// The window manager's end: messages, the fd that wakes its loop, the connection global
/// shortcut presses go out on, and the shortcut registry (it reserves the config's keys).
pub struct Bridge {
    pub rx: Receiver<ToWm>,
    pub wake: UnixStream,
    pub wm: Wm,
    pub conn: Arc<OnceLock<zbus::Connection>>,
    pub registry: Arc<Mutex<Registry>>,
}

impl Bridge {
    pub fn new() -> std::io::Result<Self> {
        let (wake, waker) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        waker.set_nonblocking(true)?;
        let (tx, rx) = mpsc::channel();
        let mut registry = match registry_path() {
            Some(path) => {
                let (registry, err) = Registry::load(path);
                if let Some(err) = err {
                    warn!("global shortcuts: {err}");
                }
                registry
            }
            None => Registry::default(),
        };
        // Launch keys from desktop files (Spectacle's Print and friends), as kglobalacceld.
        registry.add_services(newm_kde::service::scan(&newm_kde::service::dirs()));
        Ok(Self {
            rx,
            wake,
            wm: Wm {
                tx,
                waker: Arc::new(waker),
            },
            conn: Arc::new(OnceLock::new()),
            registry: Arc::new(Mutex::new(registry)),
        })
    }

    pub fn drain_wake(&self) {
        let mut buf = [0u8; 64];
        while (&self.wake).read(&mut buf).is_ok_and(|n| n > 0) {}
    }
}

/// `$XDG_STATE_HOME/newm-river/kglobalshortcuts.json`.
pub fn registry_path() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("newm-river").join("kglobalshortcuts.json"))
}

/// Asks logind to lock this session, as `loginctl lock-session` does: swayidle starts the
/// locker on logind's Lock signal.
async fn lock_session() -> fdo::Result<()> {
    let id = std::env::var("XDG_SESSION_ID")
        .map_err(|_| fdo::Error::Failed(String::from("XDG_SESSION_ID is unset")))?;
    let conn = zbus::Connection::system()
        .await
        .map_err(|e| fdo::Error::Failed(format!("no system bus: {e}")))?;
    conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "LockSession",
        &(id,),
    )
    .await
    .map_err(|e| fdo::Error::Failed(format!("logind refused to lock: {e}")))?;
    Ok(())
}

/// The interface an inhibition comes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Via {
    ScreenSaver,
    PolicyAgent,
}

/// Who holds a cookie, through which interface, and its PolicyAgent policies.
#[derive(Clone, Debug)]
struct Holder {
    via: Via,
    owner: String,
    policies: Option<u32>,
}

/// The inhibitions ScreenSaver and PolicyAgent hold, in one cookie space; the window manager
/// hears when idle is held off or let go. A release drops only a cookie its caller holds
/// through the same interface: PolicyAgent's `ReleaseInhibition` never drops a ScreenSaver
/// inhibit, nor one client's call another's. Locks go `holders`, then `cookies`.
#[derive(Clone)]
struct Inhibits {
    cookies: Arc<Mutex<Cookies<String>>>,
    holders: Arc<Mutex<BTreeMap<u32, Holder>>>,
    held: Arc<AtomicBool>,
    wm: Wm,
}

impl Inhibits {
    fn new(wm: Wm) -> Self {
        Self {
            cookies: Arc::default(),
            holders: Arc::default(),
            held: Arc::default(),
            wm,
        }
    }

    fn add(
        &self,
        via: Via,
        owner: String,
        app: String,
        reason: String,
        policies: Option<u32>,
    ) -> Option<u32> {
        let idle = policies.is_none_or(holds_off_idle);
        let mut holders = self.holders.lock().unwrap();
        let cookie = self.cookies.lock().unwrap().add(Hold {
            owner: owner.clone(),
            app,
            reason,
            idle,
        })?;
        holders.insert(
            cookie,
            Holder {
                via,
                owner,
                policies,
            },
        );
        drop(holders);
        self.sync();
        Some(cookie)
    }

    /// Drops `cookie` if `owner` holds it through `via`; answers whether it did.
    fn remove(&self, via: Via, owner: &str, cookie: u32) -> bool {
        let mut holders = self.holders.lock().unwrap();
        if !holders
            .get(&cookie)
            .is_some_and(|h| h.via == via && h.owner == owner)
        {
            return false;
        }
        holders.remove(&cookie);
        let removed = self.cookies.lock().unwrap().remove(cookie);
        drop(holders);
        self.sync();
        removed
    }

    /// Drops every hold of the clients that left the bus.
    fn remove_owners(&self, gone: &[String]) {
        let mut holders = self.holders.lock().unwrap();
        holders.retain(|_, h| !gone.contains(&h.owner));
        let mut cookies = self.cookies.lock().unwrap();
        for owner in gone {
            cookies.remove_owner(owner);
        }
        drop((cookies, holders));
        self.sync();
    }

    /// Whether a PolicyAgent inhibition holds any of the `types` policies.
    fn has_policy(&self, types: u32) -> bool {
        self.holders
            .lock()
            .unwrap()
            .values()
            .filter_map(|h| h.policies)
            .any(|p| p & types != 0)
    }

    fn sync(&self) {
        let idle = self.cookies.lock().unwrap().idle_inhibited();
        if self.held.swap(idle, Ordering::SeqCst) != idle {
            debug!("D-Bus inhibitions hold idle off: {idle}");
            self.wm.send(ToWm::Inhibit(idle));
        }
    }
}

fn sender(hdr: &Header<'_>) -> fdo::Result<String> {
    hdr.sender()
        .map(|s| s.to_string())
        .ok_or_else(|| fdo::Error::Failed(String::from("no sender")))
}

#[derive(Clone)]
struct ScreenSaver {
    inhibits: Inhibits,
    scene: Arc<Mutex<Scene>>,
    locked_since: Arc<Mutex<Option<Instant>>>,
    throttles: Arc<AtomicU32>,
}

#[interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    fn inhibit(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        application_name: String,
        reason_for_inhibit: String,
    ) -> fdo::Result<u32> {
        let owner = sender(&hdr)?;
        self.inhibits
            .add(
                Via::ScreenSaver,
                owner,
                application_name,
                reason_for_inhibit,
                None,
            )
            .ok_or_else(|| fdo::Error::Failed(String::from("no available cookie")))
    }

    fn un_inhibit(&self, #[zbus(header)] hdr: Header<'_>, cookie: u32) -> fdo::Result<()> {
        if self
            .inhibits
            .remove(Via::ScreenSaver, &sender(&hdr)?, cookie)
        {
            Ok(())
        } else {
            Err(fdo::Error::Failed(String::from("invalid cookie")))
        }
    }

    /// `true` locks the session; the lock screen cannot be turned off from here.
    async fn set_active(&self, active: bool) -> fdo::Result<bool> {
        if !active {
            return Ok(false);
        }
        lock_session().await?;
        Ok(true)
    }

    async fn lock(&self) -> fdo::Result<()> {
        lock_session().await
    }

    fn get_active(&self) -> bool {
        self.scene.lock().unwrap().locked
    }

    fn get_active_time(&self) -> u32 {
        let since = *self.locked_since.lock().unwrap();
        since.map_or(0, |t| {
            u32::try_from(t.elapsed().as_secs()).unwrap_or(u32::MAX)
        })
    }

    /// river keeps the idle time and tells the window manager nothing of it.
    fn get_session_idle_time(&self) -> fdo::Result<u32> {
        Err(fdo::Error::NotSupported(String::from(
            "river keeps the idle time; ext-idle-notify-v1 has it",
        )))
    }

    /// river's idle timer belongs to river: a D-Bus client cannot reset it.
    fn simulate_user_activity(&self) {}

    fn throttle(&self, _application_name: String, _reason_for_inhibit: String) -> u32 {
        self.throttles.fetch_add(1, Ordering::SeqCst).max(1)
    }

    fn un_throttle(&self, _cookie: u32) {}

    #[zbus(signal)]
    async fn active_changed(emitter: &SignalEmitter<'_>, active: bool) -> zbus::Result<()>;
}

#[derive(Clone)]
struct PolicyAgent {
    inhibits: Inhibits,
}

#[interface(name = "org.kde.Solid.PowerManagement.PolicyAgent")]
impl PolicyAgent {
    async fn add_inhibition(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        types: u32,
        app_name: String,
        reason: String,
    ) -> fdo::Result<u32> {
        debug!("PolicyAgent inhibition {types:#x} from {app_name:?}: {reason:?}");
        let owner = sender(&hdr)?;
        let cookie = self
            .inhibits
            .add(
                Via::PolicyAgent,
                owner,
                app_name.clone(),
                reason.clone(),
                Some(types),
            )
            .ok_or_else(|| fdo::Error::Failed(String::from("no available cookie")))?;
        if let Err(e) = Self::inhibitions_changed(&emitter, &[(app_name, reason)], &[]).await {
            warn!("InhibitionsChanged: {e}");
        }
        Ok(cookie)
    }

    async fn release_inhibition(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        cookie: u32,
    ) {
        let Ok(owner) = sender(&hdr) else {
            return;
        };
        if self.inhibits.remove(Via::PolicyAgent, &owner, cookie)
            && let Err(e) = Self::inhibitions_changed(&emitter, &[], &[]).await
        {
            warn!("InhibitionsChanged: {e}");
        }
    }

    fn list_inhibitions(&self) -> Vec<(String, String)> {
        self.inhibits.cookies.lock().unwrap().list()
    }

    fn has_inhibition(&self, types: u32) -> bool {
        self.inhibits.has_policy(types)
    }

    #[zbus(signal)]
    async fn inhibitions_changed(
        emitter: &SignalEmitter<'_>,
        added: &[(String, String)],
        removed: &[String],
    ) -> zbus::Result<()>;
}

/// One `QKeySequence` on the bus: `(ai)`, four combinations.
type Wire = (Vec<i32>,);
/// One `KGlobalShortcutInfo`: `(ssssssaiai)`.
type WireInfo = (
    String,
    String,
    String,
    String,
    String,
    String,
    Vec<i32>,
    Vec<i32>,
);

fn seq_from_wire((keys,): &Wire) -> Seq {
    let mut seq = reg::EMPTY;
    for (slot, key) in seq.iter_mut().zip(keys) {
        *slot = *key;
    }
    seq
}

fn seqs_from_wire(keys: &[Wire]) -> Vec<Seq> {
    keys.iter().map(seq_from_wire).collect()
}

fn seqs_to_wire(seqs: Vec<Seq>) -> Vec<Wire> {
    seqs.into_iter().map(|seq| (seq.to_vec(),)).collect()
}

fn info_to_wire(i: reg::Info) -> WireInfo {
    (
        i.context_unique,
        i.context_friendly,
        i.component_unique,
        i.component_friendly,
        i.unique,
        i.friendly,
        i.keys,
        i.default_keys,
    )
}

fn object_path(path: String) -> OwnedObjectPath {
    // component_path() only ever makes valid paths.
    OwnedObjectPath::try_from(path).unwrap()
}

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.kde.kglobalaccel")]
enum AccelError {
    #[zbus(error)]
    ZBus(zbus::Error),
    NoSuchComponent(String),
}

/// The `/kglobalaccel` object.
#[derive(Clone)]
struct KGlobalAccel {
    registry: Arc<Mutex<Registry>>,
    served: Arc<Mutex<BTreeSet<String>>>,
    wm: Wm,
}

/// A `/component/<name>` object.
struct ComponentObject {
    unique: String,
    service: KGlobalAccel,
}

impl KGlobalAccel {
    fn read<T>(&self, f: impl FnOnce(&Registry) -> T) -> T {
        f(&self.registry.lock().unwrap())
    }

    fn write<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> T {
        f(&mut self.registry.lock().unwrap())
    }

    /// After a call that may have changed the registry: save it, hand the window manager the
    /// keys to bind, and keep one object per component.
    async fn changed(&self, server: &ObjectServer) {
        let (added, removed) = self.commit();
        for unique in added {
            let object = ComponentObject {
                unique: unique.clone(),
                service: self.clone(),
            };
            if let Err(e) = server.at(reg::component_path(&unique), object).await {
                warn!("serving the kglobalaccel component {unique}: {e}");
            }
        }
        for unique in removed {
            if let Err(e) = server
                .remove::<ComponentObject, _>(reg::component_path(&unique))
                .await
            {
                warn!("removing the kglobalaccel component {unique}: {e}");
            }
        }
    }

    /// The part of [`Self::changed`] before its awaits, all under the registry lock: save the
    /// registry, send the window manager its bindings, and answer the component objects to add
    /// and remove. Concurrent calls thus send their bindings in the order they changed the
    /// registry, and a set computed before an await never lands after a newer one.
    fn commit(&self) -> (Vec<String>, Vec<String>) {
        self.write(|r| {
            if let Err(e) = r.flush() {
                warn!("saving the global shortcuts: {e}");
            }
            self.wm.send(ToWm::Bindings(r.bindings()));
            let now: BTreeSet<String> = r.components().map(|(u, _)| u.clone()).collect();
            let mut served = self.served.lock().unwrap();
            let added: Vec<String> = now.difference(&served).cloned().collect();
            let removed: Vec<String> = served.difference(&now).cloned().collect();
            *served = now;
            (added, removed)
        })
    }

    async fn set_keys(
        &self,
        server: &ObjectServer,
        id: &[String],
        keys: &[Seq],
        flags: u32,
    ) -> Vec<Seq> {
        let keys = self.write(|r| r.set_shortcut_keys(id, keys, flags));
        self.changed(server).await;
        keys
    }

    async fn set_foreign(
        &self,
        server: &ObjectServer,
        emitter: &SignalEmitter<'_>,
        id: &[String],
        keys: &[Seq],
    ) {
        let Some(keys) = self.write(|r| r.set_foreign_shortcut_keys(id, keys)) else {
            return;
        };
        self.changed(server).await;
        if let Err(e) = Self::your_shortcuts_changed(emitter, id, &seqs_to_wire(keys)).await {
            warn!("yourShortcutsChanged: {e}");
        }
    }
}

#[interface(name = "org.kde.KGlobalAccel")]
impl KGlobalAccel {
    #[zbus(name = "allComponents")]
    fn all_components(&self) -> Vec<OwnedObjectPath> {
        self.read(|r| r.all_component_paths())
            .into_iter()
            .map(object_path)
            .collect()
    }

    #[zbus(name = "allMainComponents")]
    fn all_main_components(&self) -> Vec<Vec<String>> {
        self.read(|r| r.all_main_components())
    }

    #[zbus(name = "allActionsForComponent")]
    fn all_actions_for_component(&self, action_id: Vec<String>) -> Vec<Vec<String>> {
        self.read(|r| r.all_actions_for_component(&action_id))
    }

    #[zbus(name = "action")]
    fn action(&self, key: i32) -> Vec<String> {
        self.read(|r| r.action_for_key(&[key, 0, 0, 0]))
    }

    #[zbus(name = "getComponent")]
    fn get_component(&self, component_unique: String) -> Result<OwnedObjectPath, AccelError> {
        self.read(|r| r.component_path(&component_unique))
            .map(object_path)
            .map_err(AccelError::NoSuchComponent)
    }

    #[zbus(name = "shortcut")]
    fn shortcut(&self, action_id: Vec<String>) -> Vec<i32> {
        reg::keys_from_seqs(&self.read(|r| r.shortcut_keys(&action_id)))
    }

    #[zbus(name = "defaultShortcut")]
    fn default_shortcut(&self, action_id: Vec<String>) -> Vec<i32> {
        reg::keys_from_seqs(&self.read(|r| r.default_shortcut_keys(&action_id)))
    }

    #[zbus(name = "setShortcut")]
    async fn set_shortcut(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        action_id: Vec<String>,
        keys: Vec<i32>,
        flags: u32,
    ) -> Vec<i32> {
        let seqs = reg::seqs_from_keys(&keys);
        reg::keys_from_seqs(&self.set_keys(server, &action_id, &seqs, flags).await)
    }

    #[zbus(name = "setForeignShortcut")]
    async fn set_foreign_shortcut(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        action_id: Vec<String>,
        keys: Vec<i32>,
    ) {
        let seqs = reg::seqs_from_keys(&keys);
        self.set_foreign(server, &emitter, &action_id, &seqs).await;
    }

    #[zbus(name = "setInactive")]
    async fn set_inactive(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        action_id: Vec<String>,
    ) {
        self.write(|r| r.set_inactive(&action_id));
        self.changed(server).await;
    }

    #[zbus(name = "doRegister")]
    async fn do_register(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        action_id: Vec<String>,
    ) {
        self.write(|r| r.do_register(&action_id));
        self.changed(server).await;
    }

    #[zbus(name = "unRegister")]
    async fn un_register(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        action_id: Vec<String>,
    ) {
        if let [component, action, ..] = action_id.as_slice() {
            self.write(|r| r.unregister(component, action));
            self.changed(server).await;
        }
    }

    #[zbus(name = "activateGlobalShortcutContext")]
    fn activate_global_shortcut_context(&self, _component: String, _context: String) {}

    #[zbus(name = "getGlobalShortcutsByKey")]
    fn get_global_shortcuts_by_key(&self, key: i32) -> Vec<WireInfo> {
        let infos = self.read(|r| r.infos_for_key(&[key, 0, 0, 0]));
        infos.into_iter().map(info_to_wire).collect()
    }

    #[zbus(name = "isGlobalShortcutAvailable")]
    fn is_global_shortcut_available(&self, key: i32, component: String) -> bool {
        self.read(|r| r.is_available(&[key, 0, 0, 0], &component))
    }

    #[zbus(name = "unregister")]
    async fn unregister(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        component_unique: String,
        shortcut_unique: String,
    ) -> bool {
        let removed = self.write(|r| r.unregister(&component_unique, &shortcut_unique));
        self.changed(server).await;
        removed
    }

    #[zbus(name = "blockGlobalShortcuts")]
    async fn block_global_shortcuts(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        block: bool,
    ) {
        self.write(|r| r.block(block));
        self.changed(server).await;
    }

    #[zbus(signal, name = "yourShortcutsChanged")]
    async fn your_shortcuts_changed(
        emitter: &SignalEmitter<'_>,
        action_id: &[String],
        new_keys: &[Wire],
    ) -> zbus::Result<()>;

    #[zbus(name = "shortcutKeys")]
    fn shortcut_keys(&self, action_id: Vec<String>) -> Vec<Wire> {
        seqs_to_wire(self.read(|r| r.shortcut_keys(&action_id)))
    }

    #[zbus(name = "defaultShortcutKeys")]
    fn default_shortcut_keys(&self, action_id: Vec<String>) -> Vec<Wire> {
        seqs_to_wire(self.read(|r| r.default_shortcut_keys(&action_id)))
    }

    #[zbus(name = "actionList")]
    fn action_list(&self, key: Wire) -> Vec<String> {
        self.read(|r| r.action_for_key(&seq_from_wire(&key)))
    }

    #[zbus(name = "setShortcutKeys")]
    async fn set_shortcut_keys(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        action_id: Vec<String>,
        keys: Vec<Wire>,
        flags: u32,
    ) -> Vec<Wire> {
        let seqs = seqs_from_wire(&keys);
        seqs_to_wire(self.set_keys(server, &action_id, &seqs, flags).await)
    }

    #[zbus(name = "setForeignShortcutKeys")]
    async fn set_foreign_shortcut_keys(
        &self,
        #[zbus(object_server)] server: &ObjectServer,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        action_id: Vec<String>,
        keys: Vec<Wire>,
    ) {
        let seqs = seqs_from_wire(&keys);
        self.set_foreign(server, &emitter, &action_id, &seqs).await;
    }

    #[zbus(name = "globalShortcutsByKey")]
    fn global_shortcuts_by_key(&self, key: Wire, _match_type: (i32,)) -> Vec<WireInfo> {
        let infos = self.read(|r| r.infos_for_key(&seq_from_wire(&key)));
        infos.into_iter().map(info_to_wire).collect()
    }

    #[zbus(name = "globalShortcutAvailable")]
    fn global_shortcut_available(&self, key: Wire, component: String) -> bool {
        self.read(|r| r.is_available(&seq_from_wire(&key), &component))
    }

    #[zbus(name = "setInverseShortcutActions")]
    fn set_inverse_shortcut_actions(
        &self,
        _c: String,
        _f: String,
        _b: String,
        _flags: u32,
    ) -> bool {
        true
    }
}

#[interface(name = "org.kde.kglobalaccel.Component")]
impl ComponentObject {
    #[zbus(property, name = "friendlyName")]
    fn friendly_name(&self) -> String {
        self.service
            .read(|r| r.component(&self.unique).map(|c| c.friendly.clone()))
            .unwrap_or_default()
    }

    #[zbus(property, name = "uniqueName")]
    fn unique_name(&self) -> String {
        self.unique.clone()
    }

    #[zbus(signal, name = "globalShortcutPressed")]
    async fn global_shortcut_pressed(
        emitter: &SignalEmitter<'_>,
        component_unique: &str,
        action_unique: &str,
        timestamp: i64,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "globalShortcutRepeated")]
    async fn global_shortcut_repeated(
        emitter: &SignalEmitter<'_>,
        component_unique: &str,
        action_unique: &str,
        timestamp: i64,
    ) -> zbus::Result<()>;

    #[zbus(signal, name = "globalShortcutReleased")]
    async fn global_shortcut_released(
        emitter: &SignalEmitter<'_>,
        component_unique: &str,
        action_unique: &str,
        timestamp: i64,
    ) -> zbus::Result<()>;

    #[zbus(name = "cleanUp")]
    async fn clean_up(&self, #[zbus(object_server)] server: &ObjectServer) -> bool {
        let changed = self.service.write(|r| r.clean_up(&self.unique));
        self.service.changed(server).await;
        changed
    }

    #[zbus(name = "isActive")]
    fn is_active(&self) -> bool {
        self.service.read(|r| r.is_active(&self.unique))
    }

    /// Qt declares this twice, with a context and without one, and System Settings' Shortcuts
    /// page calls the second. A zbus method without arguments never reads the body, so one method
    /// answers both forms; every action sits in the default context anyway.
    #[zbus(name = "shortcutNames")]
    fn shortcut_names(&self) -> Vec<String> {
        self.service.read(|r| r.shortcut_names(&self.unique))
    }

    /// Both of Qt's forms, as [`Self::shortcut_names`].
    #[zbus(name = "allShortcutInfos")]
    fn all_shortcut_infos(&self) -> Vec<WireInfo> {
        let infos = self.service.read(|r| r.infos(&self.unique));
        infos.into_iter().map(info_to_wire).collect()
    }

    #[zbus(name = "getShortcutContexts")]
    fn get_shortcut_contexts(&self) -> Vec<String> {
        vec![reg::DEFAULT_CONTEXT.to_owned()]
    }

    #[zbus(name = "invokeShortcut")]
    async fn invoke_shortcut(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        action_name: String,
    ) {
        if !self
            .service
            .read(|r| r.has_action(&self.unique, &action_name))
        {
            return;
        }
        if let Err(e) =
            Self::global_shortcut_pressed(&emitter, &self.unique, &action_name, now_ms()).await
        {
            warn!("globalShortcutPressed: {e}");
        }
    }
}

/// CLOCK_MONOTONIC in milliseconds, the clock input events carry.
pub fn now_ms() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a valid timespec for clock_gettime to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

/// Tells the app that holds `component`'s shortcut that its key went down or up.
pub fn emit_shortcut(conn: &zbus::Connection, component: &str, action: &str, pressed: bool) {
    let emitter = match SignalEmitter::new(conn, reg::component_path(component)) {
        Ok(e) => e,
        Err(e) => {
            warn!("kglobalaccel signal: {e}");
            return;
        }
    };
    let res = zbus::block_on(async {
        if pressed {
            ComponentObject::global_shortcut_pressed(&emitter, component, action, now_ms()).await
        } else {
            ComponentObject::global_shortcut_released(&emitter, component, action, now_ms()).await
        }
    });
    if let Err(e) = res {
        warn!("kglobalaccel shortcut: {e}");
    }
}

/// Starts a desktop-file component's action as kglobalacceld does:
/// `org.freedesktop.Application` for a D-Bus activatable app, on a thread of its own so a slow
/// activation never holds the window manager, else its `Exec` line.
pub fn start_service(conn: &zbus::Connection, launch: newm_kde::service::Launch) {
    use newm_kde::service::Launch;
    match launch {
        Launch::Exec(argv) => crate::river::spawn(&argv, &[]),
        Launch::DBus { bus, path, action } => {
            let conn = conn.clone();
            let res = std::thread::Builder::new()
                .name("kde-launch".to_owned())
                .spawn(move || {
                    let platform: std::collections::HashMap<&str, Variant<'_>> =
                        std::collections::HashMap::new();
                    let iface = Some("org.freedesktop.Application");
                    let res = zbus::block_on(async {
                        match &action {
                            Some(a) => {
                                let body = (a.as_str(), Vec::<Variant<'_>>::new(), platform);
                                conn.call_method(
                                    Some(bus.as_str()),
                                    path.as_str(),
                                    iface,
                                    "ActivateAction",
                                    &body,
                                )
                                .await
                            }
                            None => {
                                conn.call_method(
                                    Some(bus.as_str()),
                                    path.as_str(),
                                    iface,
                                    "Activate",
                                    &(platform,),
                                )
                                .await
                            }
                        }
                    });
                    if let Err(e) = res {
                        warn!("global shortcut: starting {bus} {action:?}: {e}");
                    }
                });
            if let Err(e) = res {
                warn!("global shortcut: launch thread: {e}");
            }
        }
    }
}

/// KRunner's window search on `org.kde.KWin`: the window manager answers the windows, a run
/// focuses one.
pub struct WindowsRunner {
    wm: Wm,
}

/// One `(sssida{sv})`.
type WireMatch = (
    String,
    String,
    String,
    i32,
    f64,
    HashMap<String, OwnedVariant>,
);

/// How long a `Match` waits for the window manager's list.
const WINDOWS_WAIT: Duration = Duration::from_millis(500);

impl WindowsRunner {
    pub fn new(wm: Wm) -> Self {
        Self { wm }
    }
}

fn match_to_wire(m: krunner::Match) -> WireMatch {
    let mut properties = HashMap::new();
    for (key, value) in [("subtext", m.subtext.as_str()), ("category", "Windows")] {
        if let Ok(v) = Variant::from(value).try_to_owned() {
            properties.insert(key.to_owned(), v);
        }
    }
    (
        m.id,
        m.text,
        m.icon,
        m.category_relevance,
        m.relevance,
        properties,
    )
}

#[interface(name = "org.kde.krunner1")]
impl WindowsRunner {
    fn actions(&self) -> Vec<(String, String, String)> {
        Vec::new()
    }

    fn config(&self) -> HashMap<String, OwnedVariant> {
        let mut config = HashMap::new();
        if let Ok(v) = Variant::from(2i32).try_to_owned() {
            config.insert(String::from("MinLetterCount"), v);
        }
        config
    }

    #[zbus(name = "Match")]
    fn match_query(&self, query: String) -> Vec<WireMatch> {
        let (tx, rx) = mpsc::channel();
        self.wm.send(ToWm::Windows(tx));
        let windows = rx.recv_timeout(WINDOWS_WAIT).unwrap_or_default();
        krunner::matches(&query, &windows)
            .into_iter()
            .map(match_to_wire)
            .collect()
    }

    fn run(&self, match_id: String, _action_id: String) {
        if let Some(id) = krunner::window_of(&match_id) {
            self.wm.send(ToWm::Activate(id));
        }
    }

    fn teardown(&self) {}
}

/// Serves ScreenSaver, PolicyAgent and kglobalaccel on one session-bus connection from a
/// thread of its own, and watches the lock (ActiveChanged) and the inhibitors' clients (a client
/// that leaves the bus drops its inhibitions). Logs and gives up without a session bus; a name
/// another program owns is logged and skipped.
pub fn serve(bridge: &Bridge, scene: Arc<Mutex<Scene>>) {
    let inhibits = Inhibits::new(bridge.wm.clone());
    let locked_since = Arc::new(Mutex::new(None));
    let saver = ScreenSaver {
        inhibits: inhibits.clone(),
        scene: scene.clone(),
        locked_since: locked_since.clone(),
        throttles: Arc::new(AtomicU32::new(1)),
    };
    let agent = PolicyAgent {
        inhibits: inhibits.clone(),
    };
    let accel = KGlobalAccel {
        registry: bridge.registry.clone(),
        served: Arc::default(),
        wm: bridge.wm.clone(),
    };
    let conn_slot = bridge.conn.clone();
    let res = thread::Builder::new()
        .name("kde-dbus".to_owned())
        .spawn(move || {
            let served = zbus::block_on(async move {
                let conn = zbus::connection::Builder::session()
                    .serve_at("/org/freedesktop/ScreenSaver", saver.clone())
                    .serve_at("/ScreenSaver", saver)
                    .serve_at("/org/kde/Solid/PowerManagement/PolicyAgent", agent)
                    .serve_at("/kglobalaccel", accel.clone())
                    .build()
                    .await?;
                let saved: Vec<String> =
                    accel.read(|r| r.components().map(|(u, _)| u.clone()).collect());
                for unique in &saved {
                    let object = ComponentObject {
                        unique: unique.clone(),
                        service: accel.clone(),
                    };
                    conn.object_server()
                        .at(reg::component_path(unique), object)
                        .await?;
                }
                *accel.served.lock().unwrap() = saved.into_iter().collect();
                for name in [
                    "org.freedesktop.ScreenSaver",
                    "org.kde.Solid.PowerManagement.PolicyAgent",
                    // xdg-desktop-portal-kde sends AddInhibition to PowerDevil's main name, at the
                    // PolicyAgent path (inhibit.cpp): without it the Inhibit portal held nothing.
                    "org.kde.Solid.PowerManagement",
                    "org.kde.kglobalaccel",
                ] {
                    match conn.request_name(name).await {
                        Ok(_) => info!("serving {name}"),
                        Err(e) => warn!("{name} not served: {e}"),
                    }
                }
                let _ = conn_slot.set(conn.clone());
                let watched = conn.clone();
                thread::Builder::new()
                    .name("kde-watch".to_owned())
                    .spawn(move || watch(&watched, &scene, &locked_since, &inhibits))
                    .map_err(|e| zbus::Error::Failure(format!("kde-watch thread: {e}")))?;
                std::future::pending::<()>().await;
                Ok::<(), zbus::Error>(())
            });
            if let Err(e) = served {
                warn!("KDE services not served: {e}");
            }
        });
    if let Err(e) = res {
        warn!("kde-dbus thread: {e}");
    }
}

/// How often the watcher looks at the lock and at the inhibitors' clients.
const WATCH: Duration = Duration::from_millis(500);

fn watch(
    conn: &zbus::Connection,
    scene: &Mutex<Scene>,
    locked_since: &Mutex<Option<Instant>>,
    inhibits: &Inhibits,
) {
    let mut locked = false;
    let mut tick = 0u32;
    loop {
        thread::sleep(WATCH);
        tick = tick.wrapping_add(1);
        let now_locked = scene.lock().unwrap().locked;
        if now_locked != locked {
            locked = now_locked;
            *locked_since.lock().unwrap() = locked.then(Instant::now);
            for path in ["/org/freedesktop/ScreenSaver", "/ScreenSaver"] {
                if let Ok(emitter) = SignalEmitter::new(conn, path)
                    && let Err(e) = zbus::block_on(ScreenSaver::active_changed(&emitter, locked))
                {
                    warn!("ActiveChanged: {e}");
                }
            }
        }
        if !tick.is_multiple_of(4) {
            continue;
        }
        let owners = inhibits.cookies.lock().unwrap().owners();
        if owners.is_empty() {
            continue;
        }
        let gone: Vec<String> = zbus::block_on(async {
            let Ok(proxy) = fdo::DBusProxy::new(conn).await else {
                return Vec::new();
            };
            let mut gone = Vec::new();
            for owner in owners {
                let Ok(name) = zbus::names::BusName::try_from(owner.as_str()) else {
                    continue;
                };
                if !proxy.name_has_owner(name).await.unwrap_or(true) {
                    gone.push(owner);
                }
            }
            gone
        });
        if !gone.is_empty() {
            debug!("inhibitors left the bus: {gone:?}");
            inhibits.remove_owners(&gone);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_sequence_pads_to_four_and_back() {
        assert_eq!(seq_from_wire(&(vec![0x0100_0009],)), [0x0100_0009, 0, 0, 0]);
        assert_eq!(seqs_to_wire(vec![[1, 2, 0, 0]]), vec![(vec![1, 2, 0, 0],)]);
    }

    #[test]
    fn inhibitions_tell_the_window_manager_only_when_idle_flips() {
        let bridge = Bridge::new().unwrap();
        let inhibits = Inhibits::new(bridge.wm.clone());
        let a = inhibits
            .add(
                Via::ScreenSaver,
                ":1.2".into(),
                "vlc".into(),
                "video".into(),
                None,
            )
            .unwrap();
        let b = inhibits
            .add(
                Via::PolicyAgent,
                ":1.3".into(),
                "portal".into(),
                "idle".into(),
                Some(newm_kde::inhibit::CHANGE_SCREEN_SETTINGS),
            )
            .unwrap();
        let c = inhibits
            .add(
                Via::PolicyAgent,
                ":1.3".into(),
                "portal".into(),
                "profile".into(),
                Some(newm_kde::inhibit::CHANGE_PROFILE),
            )
            .unwrap();
        assert!(inhibits.remove(Via::ScreenSaver, ":1.2", a));
        assert!(inhibits.remove(Via::PolicyAgent, ":1.3", b));
        assert!(inhibits.remove(Via::PolicyAgent, ":1.3", c));
        let sent: Vec<bool> = bridge
            .rx
            .try_iter()
            .filter_map(|m| match m {
                ToWm::Inhibit(on) => Some(on),
                _ => None,
            })
            .collect();
        assert_eq!(sent, vec![true, false]);
    }

    #[test]
    fn a_release_drops_only_the_callers_cookie_through_the_same_interface() {
        let bridge = Bridge::new().unwrap();
        let inhibits = Inhibits::new(bridge.wm.clone());
        let screen = newm_kde::inhibit::CHANGE_SCREEN_SETTINGS;
        let video = inhibits
            .add(
                Via::ScreenSaver,
                ":1.2".into(),
                "vlc".into(),
                "video".into(),
                None,
            )
            .unwrap();
        let portal = inhibits
            .add(
                Via::PolicyAgent,
                ":1.3".into(),
                "portal".into(),
                "idle".into(),
                Some(screen),
            )
            .unwrap();
        // ReleaseInhibition with the ScreenSaver cookie, from its holder or another client, and
        // UnInhibit from a client that holds neither cookie through ScreenSaver.
        assert!(!inhibits.remove(Via::PolicyAgent, ":1.2", video));
        assert!(!inhibits.remove(Via::PolicyAgent, ":1.3", video));
        assert!(!inhibits.remove(Via::ScreenSaver, ":1.3", video));
        assert!(!inhibits.remove(Via::ScreenSaver, ":1.3", portal));
        assert_eq!(inhibits.cookies.lock().unwrap().len(), 2);
        assert!(inhibits.remove(Via::ScreenSaver, ":1.2", video));
        assert!(!inhibits.remove(Via::ScreenSaver, ":1.2", video));
        assert!(inhibits.has_policy(screen));
        // A client leaving the bus takes its policies along.
        inhibits.remove_owners(&[":1.3".into()]);
        assert!(!inhibits.has_policy(screen));
        assert!(inhibits.cookies.lock().unwrap().is_empty());
    }

    #[test]
    fn bindings_reach_the_window_manager_in_the_order_the_registry_changed() {
        let dir = std::env::temp_dir().join(format!("newm-river-kga-{}", std::process::id()));
        let bridge = Bridge::new().unwrap();
        let (registry, _) = Registry::load(dir.join("kglobalshortcuts.json"));
        let first = KGlobalAccel {
            registry: Arc::new(Mutex::new(registry)),
            served: Arc::default(),
            wm: bridge.wm.clone(),
        };
        let second = first.clone();
        let bind = |kga: &KGlobalAccel, action: &str, key: i32| {
            let id = ["app", action, "App", action].map(String::from);
            kga.write(|r| {
                r.do_register(&id);
                r.set_shortcut_keys(
                    &id,
                    &[[key, 0, 0, 0]],
                    reg::SET_PRESENT | reg::NO_AUTOLOADING,
                );
            });
            kga.commit()
        };
        // The first call adds a component, whose object it then awaits; the second changes the
        // registry meanwhile and has nothing to await.
        assert_eq!(
            bind(&first, "one", 0x41),
            (vec![String::from("app")], vec![])
        );
        assert_eq!(bind(&second, "two", 0x42), (vec![], vec![]));
        let sent: Vec<Vec<i32>> = bridge
            .rx
            .try_iter()
            .filter_map(|m| match m {
                ToWm::Bindings(b) => Some(b.iter().map(|b| b.key).collect()),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [vec![0x41], vec![0x41, 0x42]]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_match_carries_its_subtext_and_category() {
        let m = krunner::matches(
            "thunder",
            &[krunner::Window {
                id: 3,
                title: String::from("Inbox - Thunderbird"),
                app_id: String::from("thunderbird"),
            }],
        );
        let (id, _, icon, category, _, properties) = match_to_wire(m[0].clone());
        assert_eq!((id.as_str(), icon.as_str()), ("window-3", "thunderbird"));
        assert_eq!(category, krunner::HIGH);
        assert_eq!(
            String::try_from(properties["category"].clone()).unwrap(),
            "Windows"
        );
    }
}
