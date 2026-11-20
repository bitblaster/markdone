//! The desktop's light/dark preference on Linux/BSD, from the XDG desktop portal
//! (`org.freedesktop.portal.Settings`, key `org.freedesktop.appearance` /
//! `color-scheme`: 0 no preference, 1 prefer dark, 2 prefer light). winit reports the
//! system theme only on Windows and macOS; KDE, GNOME and other portal backends publish
//! it here and signal every change.

use futures_util::StreamExt;
use zbus::{Connection, OwnedVariant, Proxy, Variant};

use crate::config::ThemeMode;

const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";

/// The desktop's preference right now (`None`: no preference, or no portal).
pub async fn current() -> Option<ThemeMode> {
    let conn = Connection::session().await.ok()?;
    read(&settings(&conn).await.ok()?).await.ok()?
}

/// Report the current preference, then every change, to `on_change` (`None` = the desktop
/// states no preference). Returns when the portal is unavailable (no session bus, no
/// portal service, an old portal without the setting) — logging why — or the connection
/// closes; the caller then simply keeps the configured mode.
pub async fn watch(mut on_change: impl FnMut(Option<ThemeMode>)) {
    if let Err(e) = watch_portal(&mut on_change).await {
        eprintln!("[markdone] system theme unavailable: {e}");
    }
}

async fn settings(conn: &Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .await
}

/// Read `color-scheme`. `ReadOne` (portal v2) returns the value; older portals only have
/// the deprecated `Read`, which wraps it in one more variant (`mode_of` unwraps nesting).
async fn read(settings: &Proxy<'_>) -> zbus::Result<Option<ThemeMode>> {
    let value: OwnedVariant = match settings.call("ReadOne", &(NAMESPACE, KEY)).await {
        Ok(v) => v,
        Err(_) => settings.call("Read", &(NAMESPACE, KEY)).await?,
    };
    Ok(mode_of(&value))
}

async fn watch_portal(on_change: &mut impl FnMut(Option<ThemeMode>)) -> zbus::Result<()> {
    let conn = Connection::session().await?;
    let settings = settings(&conn).await?;
    // Subscribe before reading, so a change between the two isn't lost.
    let mut changes = settings.receive_signal("SettingChanged").await?;
    on_change(read(&settings).await?);
    while let Some(message) = changes.next().await {
        let Ok((namespace, key, value)) = message
            .body()
            .deserialize::<(String, String, OwnedVariant)>()
        else {
            continue;
        };
        if namespace == NAMESPACE && key == KEY {
            on_change(mode_of(&value));
        }
    }
    Ok(())
}

/// The mode a `color-scheme` value asks for (`None` for "no preference" or anything
/// unexpected), unwrapping nested variants.
fn mode_of(value: &Variant) -> Option<ThemeMode> {
    match value {
        Variant::U32(1) => Some(ThemeMode::Dark),
        Variant::U32(2) => Some(ThemeMode::Light),
        Variant::Variant(inner) => mode_of(inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_scheme_values_map_to_modes() {
        assert_eq!(mode_of(&Variant::U32(1)), Some(ThemeMode::Dark));
        assert_eq!(mode_of(&Variant::U32(2)), Some(ThemeMode::Light));
        assert_eq!(mode_of(&Variant::U32(0)), None, "no preference");
        // The deprecated `Read` wraps the value in one more variant.
        let wrapped = Variant::Variant(Box::new(Variant::U32(1)));
        assert_eq!(mode_of(&wrapped), Some(ThemeMode::Dark));
    }
}
