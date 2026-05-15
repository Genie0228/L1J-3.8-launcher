//! 海底抽水功能（待實現）

use super::Toggle;
use windows::Win32::Foundation::HANDLE;

pub struct UnderwaterPump;

impl UnderwaterPump {
    pub fn new() -> Self {
        UnderwaterPump
    }
}

impl Toggle for UnderwaterPump {
    fn enable(&self, _h: HANDLE) -> anyhow::Result<()> {
        // 待實現
        Ok(())
    }

    fn disable(&self, _h: HANDLE) -> anyhow::Result<()> {
        // 待實現
        Ok(())
    }
}
