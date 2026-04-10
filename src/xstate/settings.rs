use super::XState;
use log::warn;
use std::{collections::HashMap, env, fs, path::PathBuf};
use xcb::x;

impl XState {
    pub(crate) fn set_xsettings_owner(&self) {
        self.connection
            .send_and_check_request(&x::SetSelectionOwner {
                owner: self.settings.window,
                selection: self.atoms.xsettings,
                time: x::CURRENT_TIME,
            })
            .unwrap();
        let reply = self
            .connection
            .wait_for_reply(self.connection.send_request(&x::GetSelectionOwner {
                selection: self.atoms.xsettings,
            }))
            .unwrap();

        if reply.owner() != self.settings.window {
            warn!(
                "Could not get XSETTINGS selection (owned by {:?})",
                reply.owner()
            );
        }
    }

    pub(crate) fn update_global_scale(&mut self, scale: f64) {
        self.settings.set_scale(scale);
        self.connection
            .send_and_check_request(&x::ChangeProperty {
                window: self.settings.window,
                mode: x::PropMode::Replace,
                property: self.atoms.xsettings_settings,
                r#type: self.atoms.xsettings_settings,
                data: &self.settings.as_data(),
            })
            .unwrap();
        let resource_manager = self.settings.xresources(scale);
        self.connection
            .send_and_check_request(&x::ChangeProperty {
                window: self.root,
                mode: x::PropMode::Replace,
                property: x::ATOM_RESOURCE_MANAGER,
                r#type: x::ATOM_STRING,
                data: resource_manager.as_bytes(),
            })
            .unwrap();
        self.reload_default_cursor();
    }
}

/// The DPI consider 1x scale by X11.
const DEFAULT_DPI: i32 = 96;
/// I don't know why, but the DPI related xsettings seem to
/// divide the DPI by 1024.
const DPI_SCALE_FACTOR: i32 = 1024;

const XFT_DPI: &str = "Xft/DPI";
const GDK_WINDOW_SCALE: &str = "Gdk/WindowScalingFactor";
const GDK_UNSCALED_DPI: &str = "Gdk/UnscaledDPI";
const DEFAULT_CURSOR_SIZE: i32 = 24;
const RESOURCE_MANAGER_CHUNK_LEN: u32 = 4096;

pub(super) struct Settings {
    window: x::Window,
    serial: u32,
    settings: HashMap<&'static str, IntSetting>,
    desktop_settings: DesktopSettings,
}

#[derive(Copy, Clone)]
struct IntSetting {
    value: i32,
    last_change_serial: u32,
}

#[derive(Clone)]
struct DesktopSettings {
    cursor_size: i32,
    xresources: String,
}

mod setting_type {
    pub const INTEGER: u8 = 0;
}

impl Settings {
    pub(super) fn new(connection: &xcb::Connection, atoms: &super::Atoms, root: x::Window) -> Self {
        let desktop_settings = DesktopSettings::load(connection, root);
        let window = connection.generate_id();
        connection
            .send_and_check_request(&x::CreateWindow {
                wid: window,
                width: 1,
                height: 1,
                depth: 0,
                parent: root,
                x: 0,
                y: 0,
                border_width: 0,
                class: x::WindowClass::InputOnly,
                visual: x::COPY_FROM_PARENT,
                value_list: &[],
            })
            .expect("Couldn't create window for settings");

        let s = Settings {
            window,
            serial: 0,
            settings: HashMap::from([
                (
                    XFT_DPI,
                    IntSetting {
                        value: DEFAULT_DPI * DPI_SCALE_FACTOR,
                        last_change_serial: 0,
                    },
                ),
                (
                    GDK_WINDOW_SCALE,
                    IntSetting {
                        value: 1,
                        last_change_serial: 0,
                    },
                ),
                (
                    GDK_UNSCALED_DPI,
                    IntSetting {
                        value: DEFAULT_DPI * DPI_SCALE_FACTOR,
                        last_change_serial: 0,
                    },
                ),
            ]),
            desktop_settings,
        };

        connection
            .send_and_check_request(&x::ChangeProperty {
                window,
                mode: x::PropMode::Replace,
                property: atoms.xsettings_settings,
                r#type: atoms.xsettings_settings,
                data: &s.as_data(),
            })
            .unwrap();

        s
    }

    fn as_data(&self) -> Vec<u8> {
        // https://specifications.freedesktop.org/xsettings-spec/0.5/#format

        let mut data = vec![
            // GTK seems to use this value for byte order from the X.h header,
            // so I assume I can use it too.
            x::ImageOrder::LsbFirst as u8,
            // unused
            0,
            0,
            0,
        ];

        data.extend_from_slice(&self.serial.to_le_bytes());
        data.extend_from_slice(&(self.settings.len() as u32).to_le_bytes());

        fn insert_with_padding(data: &[u8], out: &mut Vec<u8>) {
            out.extend_from_slice(data);
            // See https://x.org/releases/X11R7.7/doc/xproto/x11protocol.html#Syntactic_Conventions_b
            let num_padding_bytes = (4 - (data.len() % 4)) % 4;
            out.extend(std::iter::repeat_n(0, num_padding_bytes));
        }

        for (name, setting) in &self.settings {
            data.extend_from_slice(&[setting_type::INTEGER, 0]);
            data.extend_from_slice(&(name.len() as u16).to_le_bytes());
            insert_with_padding(name.as_bytes(), &mut data);
            data.extend_from_slice(&setting.last_change_serial.to_le_bytes());
            data.extend_from_slice(&setting.value.to_le_bytes());
        }

        data
    }

    fn set_scale(&mut self, scale: f64) {
        self.serial += 1;

        let scale = scale.max(1.0);
        let setting = IntSetting {
            value: (scale * DEFAULT_DPI as f64 * DPI_SCALE_FACTOR as f64).round() as i32,
            last_change_serial: self.serial,
        };
        self.settings.entry(XFT_DPI).insert_entry(setting);
        // Gdk/WindowScalingFactor + Gdk/UnscaledDPI is identical to setting
        // GDK_SCALE = scale and then GDK_DPI_SCALE = 1 / scale.
        self.settings
            .entry(GDK_UNSCALED_DPI)
            .insert_entry(IntSetting {
                value: setting.value / scale as i32,
                last_change_serial: self.serial,
            });
        self.settings
            .entry(GDK_WINDOW_SCALE)
            .insert_entry(IntSetting {
                value: scale as i32,
                last_change_serial: self.serial,
            });
    }

    fn xresources(&self, scale: f64) -> String {
        let mut resources = self.desktop_settings.xresources.clone();
        update_xresource_property(
            &mut resources,
            "Xcursor.size",
            &self.cursor_size(scale).to_string(),
        );
        resources
    }

    fn cursor_size(&self, scale: f64) -> i32 {
        self.desktop_settings
            .cursor_size
            .saturating_mul(integer_cursor_scale(scale))
            .max(1)
    }
}

impl DesktopSettings {
    fn load(connection: &xcb::Connection, root: x::Window) -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        let xresources = current_xresources(connection, root)
            .filter(|contents| !contents.is_empty())
            .or_else(|| {
                home.as_ref()
                    .and_then(|home| fs::read_to_string(home.join(".Xresources")).ok())
            })
            .unwrap_or_default();
        let mut cursor_size = parse_xresources(&xresources);

        for relative_path in [
            ".config/gtk-3.0/settings.ini",
            ".config/gtk-4.0/settings.ini",
            ".gtkrc-2.0",
        ] {
            if cursor_size.is_some() {
                break;
            }
            let Some(home) = home.as_ref() else {
                break;
            };
            let Ok(contents) = fs::read_to_string(home.join(relative_path)) else {
                continue;
            };
            cursor_size = cursor_size.or(parse_key_value_settings(&contents));
        }

        let cursor_size = cursor_size
            .or_else(|| {
                env::var("XCURSOR_SIZE")
                    .ok()
                    .and_then(|size| parse_cursor_size(&size))
            })
            .unwrap_or(DEFAULT_CURSOR_SIZE)
            .max(1);

        Self {
            cursor_size,
            xresources,
        }
    }
}

fn integer_cursor_scale(scale: f64) -> i32 {
    let rounded_scale = scale.round();
    if rounded_scale > 1.0 && (scale - rounded_scale).abs() < 0.01 {
        rounded_scale as i32
    } else {
        1
    }
}

fn current_xresources(connection: &xcb::Connection, root: x::Window) -> Option<String> {
    let reply = connection
        .wait_for_reply(connection.send_request(&x::GetProperty {
            delete: false,
            window: root,
            property: x::ATOM_RESOURCE_MANAGER,
            r#type: x::ATOM_STRING,
            long_offset: 0,
            long_length: RESOURCE_MANAGER_CHUNK_LEN,
        }))
        .ok()?;
    let data: &[u8] = reply.value();
    Some(String::from_utf8_lossy(data).into_owned())
}

fn parse_xresources(contents: &str) -> Option<i32> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('!') || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "Xcursor.size" {
            return parse_cursor_size(value);
        }
    }
    None
}

fn parse_key_value_settings(contents: &str) -> Option<i32> {
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "gtk-cursor-theme-size" {
            return parse_cursor_size(value);
        }
    }
    None
}

fn parse_cursor_size(value: &str) -> Option<i32> {
    value.trim().trim_matches('"').parse().ok()
}

fn update_xresource_property(resources: &mut String, key: &str, value: &str) {
    let mut updated = false;
    let mut lines = Vec::new();

    for line in resources.lines() {
        let trimmed = line.trim_start();
        if trimmed
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
        {
            lines.push(format!("{key}:\t{value}"));
            updated = true;
        } else {
            lines.push(line.to_string());
        }
    }

    if !updated {
        lines.push(format!("{key}:\t{value}"));
    }

    *resources = lines.join("\n");
    if !resources.ends_with('\n') {
        resources.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::{
        integer_cursor_scale, parse_key_value_settings, parse_xresources, update_xresource_property,
    };

    #[test]
    fn parses_cursor_size_from_xresources() {
        let parsed = parse_xresources("Xcursor.size: 24\nXcursor.theme: Bibata-Modern-Ice\n");
        assert_eq!(parsed, Some(24));
    }

    #[test]
    fn parses_cursor_size_from_gtk_config() {
        let parsed = parse_key_value_settings(
            r#"
            [Settings]
            gtk-cursor-theme-name="Bibata-Modern-Ice"
            gtk-cursor-theme-size=24
            "#,
        );
        assert_eq!(parsed, Some(24));
    }

    #[test]
    fn updates_existing_xresource_property() {
        let mut resources = "Xcursor.size:\t24\nXcursor.theme:\tBibata\n".to_string();
        update_xresource_property(&mut resources, "Xcursor.size", "48");

        assert!(resources.contains("Xcursor.size:\t48\n"));
        assert!(resources.contains("Xcursor.theme:\tBibata\n"));
    }

    #[test]
    fn only_scales_integer_outputs() {
        assert_eq!(integer_cursor_scale(2.0), 2);
        assert_eq!(integer_cursor_scale(1.5), 1);
        assert_eq!(integer_cursor_scale(1.0), 1);
    }
}
