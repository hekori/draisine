//! Linux desktop integration. On Wayland a client cannot hand its window icon to the
//! compositor; the taskbar looks up the app_id ("draisine") in a `.desktop` file and takes
//! its `Icon=`. Without one, KDE/GNOME show a generic Wayland icon. So we keep a small
//! `draisine.desktop` + logo PNG in the user's data dir (rewritten only when they change,
//! e.g. after the portable folder was moved).

#[cfg(target_os = "linux")]
pub fn ensure() {
    if let Err(e) = install() {
        crate::log::warn(format!("Desktop entry not written: {e:#}"));
    }
}

#[cfg(not(target_os = "linux"))]
pub fn ensure() {}

#[cfg(target_os = "linux")]
fn install() -> anyhow::Result<()> {
    use anyhow::Context;
    let base = directories::BaseDirs::new().context("no home directory")?;
    let data = base.data_dir(); // ~/.local/share

    let icon_path = data.join("icons/hicolor/256x256/apps/draisine.png");
    write_if_changed(&icon_path, &png(256)?)?;

    let exe = std::env::current_exe()?;
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Draisine\n\
         Comment={}\n\
         Exec=\"{}\" --window\n\
         Icon={}\n\
         Categories=Utility;Network;\n\
         StartupWMClass=draisine\n\
         Terminal=false\n",
        env!("CARGO_PKG_DESCRIPTION"),
        exe.display(),
        icon_path.display(),
    );
    write_if_changed(&data.join("applications/draisine.desktop"), entry.as_bytes())
}

#[cfg(target_os = "linux")]
fn write_if_changed(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn png(size: u32) -> anyhow::Result<Vec<u8>> {
    let px = crate::icon::rgba(size, crate::icon::Status::Idle.color());
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&px)?;
    Ok(out)
}
