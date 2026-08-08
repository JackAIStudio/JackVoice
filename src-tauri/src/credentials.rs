use keyring::{Entry, Error};

const SERVICE: &str = "com.jackvoice.shared";
const VOLC_API_KEY_ACCOUNT: &str = "volc-api-key";

fn volc_entry() -> Result<Entry, String> {
    Entry::new(SERVICE, VOLC_API_KEY_ACCOUNT)
        .map_err(|error| format!("无法访问系统凭证库：{error}"))
}

pub fn load_volc_api_key() -> Result<String, String> {
    match volc_entry()?.get_password() {
        Ok(value) => Ok(value),
        Err(Error::NoEntry) => Ok(String::new()),
        Err(error) => Err(format!("无法从系统凭证库读取豆包 APP Key：{error}")),
    }
}

pub fn save_volc_api_key(value: &str) -> Result<(), String> {
    let entry = volc_entry()?;
    let value = value.trim();
    if value.is_empty() {
        return match entry.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(error) => Err(format!("无法从系统凭证库删除豆包 APP Key：{error}")),
        };
    }
    entry
        .set_password(value)
        .map_err(|error| format!("无法把豆包 APP Key 保存到系统凭证库：{error}"))
}
