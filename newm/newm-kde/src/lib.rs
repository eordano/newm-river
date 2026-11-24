//! The KDE session services newm-river serves over D-Bus where Plasma has KWin and
//! PowerDevil: global shortcuts (`org.kde.kglobalaccel`), KRunner's window search and power
//! inhibitions; and the one name each app goes by across X11, Wayland and hosts ([`apps`]). This
//! crate holds their logic and no D-Bus library, so both compositors share it whatever zbus they
//! build with (newm-river takes a copy of it).

pub mod apps;
pub mod exec;
pub mod inhibit;
pub mod kglobalaccel;
pub mod krunner;
pub mod qtkey;
pub mod service;
