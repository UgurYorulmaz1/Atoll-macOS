// UI Automation: reaches an app's own controls for actions the Windows media interface does not support.
// Apple Music (Windows) does not allow seeking through SMTC, but its progress bar ("LCDScrubber") is writable.
use windows::core::{Result, BSTR};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::*;

pub fn apple_music_seek(sec: f64) -> Result<()> {
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let root = uia.GetRootElement()?;
        let by_name = uia.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(BSTR::from("Apple Music")))?;
        let win = root.FindFirst(TreeScope_Children, &by_name)?;
        let by_id = uia.CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(BSTR::from("LCDScrubber")))?;
        let slider = win.FindFirst(TreeScope_Descendants, &by_id)?;
        let rv: IUIAutomationRangeValuePattern = slider.GetCurrentPatternAs(UIA_RangeValuePatternId)?;
        let max = rv.CurrentMaximum().unwrap_or(f64::MAX);
        rv.SetValue(sec.clamp(0.0, max))
    }
}
