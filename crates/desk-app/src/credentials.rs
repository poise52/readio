use zeroize::Zeroizing;

const INVALID_KEY: &str =
    "Ключ OpenRouter должен содержать от 1 до 512 печатных ASCII-символов без пробелов";

pub fn validate_key(key: &str) -> Result<(), &'static str> {
    if key.is_empty() || key.len() > 512 || !key.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(INVALID_KEY);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new("app.readio.desktop.openrouter", "api-key")
        .map_err(|_| "Не удалось открыть системное хранилище ключей".into())
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn read() -> Result<Option<Zeroizing<String>>, String> {
    match entry()?.get_password() {
        Ok(key) => Ok(Some(Zeroizing::new(key))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(
            "Не удалось прочитать ключ OpenRouter. Проверьте доступ к системному хранилищу".into(),
        ),
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn write(key: &str) -> Result<(), String> {
    validate_key(key).map_err(str::to_owned)?;
    entry()?.set_password(key).map_err(|_| {
        "Не удалось сохранить ключ OpenRouter. Проверьте доступ к системному хранилищу".into()
    })
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn delete() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(
            "Не удалось удалить ключ OpenRouter. Проверьте доступ к системному хранилищу".into(),
        ),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn read() -> Result<Option<Zeroizing<String>>, String> {
    Err("Системное хранилище ключей доступно только в Windows и macOS".into())
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn write(_: &str) -> Result<(), String> {
    Err("Системное хранилище ключей доступно только в Windows и macOS".into())
}
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn delete() -> Result<(), String> {
    Err("Системное хранилище ключей доступно только в Windows и macOS".into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_empty_control_space_and_oversized_keys() {
        assert!(super::validate_key("sk-or-v1-test").is_ok());
        for key in ["", "a b", "a\n", "ключ"] {
            assert!(super::validate_key(key).is_err());
        }
        assert!(super::validate_key(&"x".repeat(513)).is_err());
    }
}
