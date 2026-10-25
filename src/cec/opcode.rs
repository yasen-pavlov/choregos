macro_rules! opcodes {
    ($($constant:ident = $value:literal => $name:literal),+ $(,)?) => {
        $(pub const $constant: u8 = $value;)+
        pub fn name(opcode: u8) -> &'static str {
            match opcode { $($value => $name,)+ _ => "Unknown" }
        }
    };
}
opcodes! {
    FEATURE_ABORT = 0x00 => "Feature Abort",
    IMAGE_VIEW_ON = 0x04 => "Image View On",
    TEXT_VIEW_ON = 0x0D => "Text View On",
    SET_MENU_LANGUAGE = 0x32 => "Set Menu Language",
    STANDBY = 0x36 => "Standby",
    USER_CONTROL_PRESSED = 0x44 => "User Control Pressed",
    USER_CONTROL_RELEASED = 0x45 => "User Control Released",
    GIVE_OSD_NAME = 0x46 => "Give OSD Name",
    SET_OSD_NAME = 0x47 => "Set OSD Name",
    SYSTEM_AUDIO_MODE_REQUEST = 0x70 => "System Audio Mode Request",
    SET_SYSTEM_AUDIO_MODE = 0x72 => "Set System Audio Mode",
    GIVE_SYSTEM_AUDIO_MODE_STATUS = 0x7D => "Give System Audio Mode Status",
    SYSTEM_AUDIO_MODE_STATUS = 0x7E => "System Audio Mode Status",
    ACTIVE_SOURCE = 0x82 => "Active Source",
    GIVE_PHYSICAL_ADDRESS = 0x83 => "Give Physical Address",
    REPORT_PHYSICAL_ADDRESS = 0x84 => "Report Physical Address",
    REQUEST_ACTIVE_SOURCE = 0x85 => "Request Active Source",
    SET_STREAM_PATH = 0x86 => "Set Stream Path",
    DEVICE_VENDOR_ID = 0x87 => "Device Vendor ID",
    GIVE_DEVICE_VENDOR_ID = 0x8C => "Give Device Vendor ID",
    MENU_STATUS = 0x8E => "Menu Status",
    GIVE_DEVICE_POWER_STATUS = 0x8F => "Give Device Power Status",
    REPORT_POWER_STATUS = 0x90 => "Report Power Status",
    GET_MENU_LANGUAGE = 0x91 => "Get Menu Language",
    INACTIVE_SOURCE = 0x9D => "Inactive Source",
    CEC_VERSION = 0x9E => "CEC Version",
    GET_CEC_VERSION = 0x9F => "Get CEC Version",
    REPORT_FEATURES = 0xA6 => "Report Features",
    REQUEST_CURRENT_LATENCY = 0xA7 => "Request Current Latency",
}
