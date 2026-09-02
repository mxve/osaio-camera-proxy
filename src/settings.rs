pub struct Setting {
    pub name: &'static str,
    pub attribute: &'static str,
}

pub const SETTINGS: &[Setting] = &[Setting {
    name: "night-vision",
    attribute: "IrLedMode",
}];

pub fn find(name: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.name == name)
}

impl Setting {
    pub fn parse(&self, text: &str) -> Option<i64> {
        match text {
            "off" | "0" => Some(0),
            "on" | "1" => Some(1),
            "auto" | "2" => Some(2),
            _ => None,
        }
    }

    pub fn label(&self, value: i64) -> &'static str {
        match value {
            0 => "off",
            1 => "on",
            2 => "auto",
            _ => "unknown",
        }
    }
}
