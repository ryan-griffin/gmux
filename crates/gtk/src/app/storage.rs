//! GTK preferences are independent of daemon configuration.
pub fn load<T: serde::de::DeserializeOwned + Default>(name: &str) -> anyhow::Result<T> {
    gmux_client::storage::load(&format!("clients/gtk/{name}"))
}
pub fn save<T: serde::Serialize + serde::de::DeserializeOwned + Default>(
    name: &str,
    value: &T,
) -> anyhow::Result<()> {
    gmux_client::storage::save(&format!("clients/gtk/{name}"), value)
}
