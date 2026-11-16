pub mod core;
#[cfg(windows)]
pub mod platform;
pub mod storage;
#[cfg(all(windows, feature = "gui"))]
pub mod ui;

#[cfg(test)]
mod tests;
