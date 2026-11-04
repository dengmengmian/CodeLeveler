//! UI Automation client helpers.
//!
//! The backend reads the control view: it answers what a person can see and
//! operate, not every layout element a provider happens to expose. Pattern
//! probes are the source of both the advertised `actions` and the value text,
//! so the tree and the action dispatcher agree about what an element supports.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement,
    IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern, IUIAutomationScrollItemPattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTogglePattern, IUIAutomationTreeWalker,
    IUIAutomationValuePattern, UIA_AppBarControlTypeId, UIA_ButtonControlTypeId,
    UIA_CONTROLTYPE_ID, UIA_CalendarControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_CustomControlTypeId, UIA_DataGridControlTypeId,
    UIA_DataItemControlTypeId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
    UIA_ExpandCollapsePatternId, UIA_GroupControlTypeId, UIA_HeaderControlTypeId,
    UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
    UIA_InvokePatternId, UIA_ListControlTypeId, UIA_ListItemControlTypeId,
    UIA_MenuBarControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
    UIA_PaneControlTypeId, UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId,
    UIA_ScrollBarControlTypeId, UIA_ScrollItemPatternId, UIA_SelectionItemPatternId,
    UIA_SemanticZoomControlTypeId, UIA_SeparatorControlTypeId, UIA_SliderControlTypeId,
    UIA_SpinnerControlTypeId, UIA_SplitButtonControlTypeId, UIA_StatusBarControlTypeId,
    UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TableControlTypeId, UIA_TextControlTypeId,
    UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId, UIA_TogglePatternId,
    UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId, UIA_TreeControlTypeId,
    UIA_TreeItemControlTypeId, UIA_ValuePatternId, UIA_WindowControlTypeId,
};
use windows::core::{BSTR, Interface};

use crate::error::{CuaError, CuaResult};
use crate::types::{Bounds, Expectation, Node};

use super::NAME;

/// Matches the macOS accessibility messaging timeout. A provider that stops
/// answering must not hold the single GUI worker forever.
const UIA_TIMEOUT_MS: u32 = 4000;

pub(super) fn automation() -> CuaResult<IUIAutomation> {
    // CUIAutomation8 carries the connection and transaction timeouts; the
    // original object is the fallback for hosts where it is unavailable.
    let modern = unsafe {
        CoCreateInstance::<_, IUIAutomation2>(
            &CUIAutomation8,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    };
    if let Ok(automation) = modern {
        unsafe {
            let _ = automation.SetConnectionTimeout(UIA_TIMEOUT_MS);
            let _ = automation.SetTransactionTimeout(UIA_TIMEOUT_MS);
        }
        return automation
            .cast()
            .map_err(|cause| error("obtain the base UI Automation interface", cause));
    }
    unsafe {
        CoCreateInstance::<_, IUIAutomation>(
            &CUIAutomation,
            None::<&windows::core::IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .map_err(|cause| error("create the UI Automation client", cause))
}

pub(super) fn error(operation: &str, error: windows::core::Error) -> CuaError {
    let code = error.code().0 as u32;
    let message = error.message();
    match code {
        0x8007_0005 => CuaError::ElevationRequired(format!(
            "{operation}: {message}; the target runs at a higher integrity level, so Windows \
             blocks this process (UIPI)"
        )),
        0x8004_0200 => CuaError::InvalidAction(format!("{operation}: the element is not enabled")),
        0x8004_0201 => {
            CuaError::ElementChanged(format!("{operation}: the element is no longer available"))
        }
        0x8004_0202 => {
            CuaError::InvalidAction(format!("{operation}: the element has no clickable point"))
        }
        _ => CuaError::Backend {
            backend: NAME,
            detail: format!("{operation}: {message} (0x{code:08X})"),
        },
    }
}

pub(super) fn root(automation: &IUIAutomation, hwnd: HWND) -> CuaResult<IUIAutomationElement> {
    unsafe { automation.ElementFromHandle(hwnd) }
        .map_err(|cause| error("read the window element", cause))
}

pub(super) fn walker(automation: &IUIAutomation) -> CuaResult<IUIAutomationTreeWalker> {
    unsafe { automation.ControlViewWalker() }
        .map_err(|cause| error("create the control-view walker", cause))
}

pub(super) fn focus(element: &IUIAutomationElement) -> CuaResult<()> {
    unsafe { element.SetFocus() }.map_err(|cause| error("focus the element", cause))
}

pub(super) fn element_bounds(element: &IUIAutomationElement) -> Option<Bounds> {
    let rect = unsafe { element.CurrentBoundingRectangle() }.ok()?;
    rect_bounds(rect)
}

fn rect_bounds(rect: RECT) -> Option<Bounds> {
    let width = f64::from(rect.right - rect.left);
    let height = f64::from(rect.bottom - rect.top);
    (width > 0.0 && height > 0.0).then_some(Bounds {
        x: f64::from(rect.left),
        y: f64::from(rect.top),
        width,
        height,
    })
}

/// The patterns observed in one read. Keeping the objects together means a
/// node's value and its advertised actions come from the same probe.
pub(super) struct Patterns {
    pub(super) invoke: Option<IUIAutomationInvokePattern>,
    pub(super) toggle: Option<IUIAutomationTogglePattern>,
    pub(super) expand: Option<IUIAutomationExpandCollapsePattern>,
    pub(super) selection: Option<IUIAutomationSelectionItemPattern>,
    pub(super) scroll: Option<IUIAutomationScrollItemPattern>,
    pub(super) value: Option<IUIAutomationValuePattern>,
}

impl Patterns {
    pub(super) fn read(element: &IUIAutomationElement) -> Self {
        unsafe {
            Self {
                invoke: element
                    .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    .ok(),
                toggle: element
                    .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    .ok(),
                expand: element
                    .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )
                    .ok(),
                selection: element
                    .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                        UIA_SelectionItemPatternId,
                    )
                    .ok(),
                scroll: element
                    .GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
                    .ok(),
                value: element
                    .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    .ok(),
            }
        }
    }

    pub(super) fn writable_value(&self) -> Option<&IUIAutomationValuePattern> {
        let value = self.value.as_ref()?;
        let read_only = unsafe { value.CurrentIsReadOnly() }
            .map(|flag| flag.as_bool())
            .unwrap_or(false);
        (!read_only).then_some(value)
    }

    pub(super) fn value_text(&self) -> Option<String> {
        self.raw_value_text().filter(|text| !text.is_empty())
    }

    /// The value without the empty-string filter, so a write of `""` can be
    /// verified as applied rather than reported as unreadable.
    pub(super) fn raw_value_text(&self) -> Option<String> {
        let value = self.value.as_ref()?;
        unsafe { value.CurrentValue() }
            .ok()
            .map(|text| text.to_string())
    }

    pub(super) fn set_value(&self, value: &str) -> CuaResult<()> {
        let Some(pattern) = self.writable_value() else {
            return Err(CuaError::InvalidAction(
                "the element's value is missing or read-only".into(),
            ));
        };
        unsafe { pattern.SetValue(&BSTR::from(value)) }
            .map_err(|cause| error("set the element value", cause))
    }

    /// Dispatches one action name from [`Self::action_names`]. `set_value` is
    /// not performed here because it needs the value the caller is writing.
    pub(super) fn perform(&self, action: &str) -> CuaResult<()> {
        let missing = || CuaError::InvalidAction(format!("the element does not expose {action:?}"));
        match action {
            "invoke" => unsafe { self.invoke.as_ref().ok_or_else(missing)?.Invoke() }
                .map_err(|cause| error("invoke the element", cause)),
            "toggle" => unsafe { self.toggle.as_ref().ok_or_else(missing)?.Toggle() }
                .map_err(|cause| error("toggle the element", cause)),
            "expand" => unsafe { self.expand.as_ref().ok_or_else(missing)?.Expand() }
                .map_err(|cause| error("expand the element", cause)),
            "collapse" => unsafe { self.expand.as_ref().ok_or_else(missing)?.Collapse() }
                .map_err(|cause| error("collapse the element", cause)),
            "select" => unsafe { self.selection.as_ref().ok_or_else(missing)?.Select() }
                .map_err(|cause| error("select the element", cause)),
            "add_to_selection" => unsafe {
                self.selection
                    .as_ref()
                    .ok_or_else(missing)?
                    .AddToSelection()
            }
            .map_err(|cause| error("add the element to the selection", cause)),
            "scroll_into_view" => {
                unsafe { self.scroll.as_ref().ok_or_else(missing)?.ScrollIntoView() }
                    .map_err(|cause| error("scroll the element into view", cause))
            }
            "set_value" => Err(CuaError::InvalidAction(
                "set_value writes a value; use the set_value action".into(),
            )),
            other => Err(CuaError::InvalidAction(format!(
                "unknown accessibility action {other:?}"
            ))),
        }
    }

    pub(super) fn action_names(&self) -> Vec<String> {
        let mut actions = Vec::new();
        if self.invoke.is_some() {
            actions.push("invoke".to_string());
        }
        if self.toggle.is_some() {
            actions.push("toggle".to_string());
        }
        if self.expand.is_some() {
            actions.push("expand".to_string());
            actions.push("collapse".to_string());
        }
        if self.selection.is_some() {
            actions.push("select".to_string());
            actions.push("add_to_selection".to_string());
        }
        if self.scroll.is_some() {
            actions.push("scroll_into_view".to_string());
        }
        if self.writable_value().is_some() {
            actions.push("set_value".to_string());
        }
        actions
    }
}

pub(super) fn control_type_name(id: i32) -> String {
    const CONTROL_TYPES: &[(UIA_CONTROLTYPE_ID, &str)] = &[
        (UIA_AppBarControlTypeId, "UIA_AppBar"),
        (UIA_ButtonControlTypeId, "UIA_Button"),
        (UIA_CalendarControlTypeId, "UIA_Calendar"),
        (UIA_CheckBoxControlTypeId, "UIA_CheckBox"),
        (UIA_ComboBoxControlTypeId, "UIA_ComboBox"),
        (UIA_CustomControlTypeId, "UIA_Custom"),
        (UIA_DataGridControlTypeId, "UIA_DataGrid"),
        (UIA_DataItemControlTypeId, "UIA_DataItem"),
        (UIA_DocumentControlTypeId, "UIA_Document"),
        (UIA_EditControlTypeId, "UIA_Edit"),
        (UIA_GroupControlTypeId, "UIA_Group"),
        (UIA_HeaderControlTypeId, "UIA_Header"),
        (UIA_HeaderItemControlTypeId, "UIA_HeaderItem"),
        (UIA_HyperlinkControlTypeId, "UIA_Hyperlink"),
        (UIA_ImageControlTypeId, "UIA_Image"),
        (UIA_ListControlTypeId, "UIA_List"),
        (UIA_ListItemControlTypeId, "UIA_ListItem"),
        (UIA_MenuBarControlTypeId, "UIA_MenuBar"),
        (UIA_MenuControlTypeId, "UIA_Menu"),
        (UIA_MenuItemControlTypeId, "UIA_MenuItem"),
        (UIA_PaneControlTypeId, "UIA_Pane"),
        (UIA_ProgressBarControlTypeId, "UIA_ProgressBar"),
        (UIA_RadioButtonControlTypeId, "UIA_RadioButton"),
        (UIA_ScrollBarControlTypeId, "UIA_ScrollBar"),
        (UIA_SemanticZoomControlTypeId, "UIA_SemanticZoom"),
        (UIA_SeparatorControlTypeId, "UIA_Separator"),
        (UIA_SliderControlTypeId, "UIA_Slider"),
        (UIA_SpinnerControlTypeId, "UIA_Spinner"),
        (UIA_SplitButtonControlTypeId, "UIA_SplitButton"),
        (UIA_StatusBarControlTypeId, "UIA_StatusBar"),
        (UIA_TabControlTypeId, "UIA_Tab"),
        (UIA_TabItemControlTypeId, "UIA_TabItem"),
        (UIA_TableControlTypeId, "UIA_Table"),
        (UIA_TextControlTypeId, "UIA_Text"),
        (UIA_ThumbControlTypeId, "UIA_Thumb"),
        (UIA_TitleBarControlTypeId, "UIA_TitleBar"),
        (UIA_ToolBarControlTypeId, "UIA_ToolBar"),
        (UIA_ToolTipControlTypeId, "UIA_ToolTip"),
        (UIA_TreeControlTypeId, "UIA_Tree"),
        (UIA_TreeItemControlTypeId, "UIA_TreeItem"),
        (UIA_WindowControlTypeId, "UIA_Window"),
    ];
    CONTROL_TYPES
        .iter()
        .find(|(control, _)| control.0 == id)
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| format!("UIA_ControlType_{id}"))
}

pub(super) struct Walk<'a> {
    walker: &'a IUIAutomationTreeWalker,
    remaining: usize,
    pub(super) truncated: bool,
    cancel: &'a AtomicBool,
    depth: u32,
}

impl<'a> Walk<'a> {
    pub(super) fn new(
        walker: &'a IUIAutomationTreeWalker,
        max_nodes: u32,
        depth: u32,
        cancel: &'a AtomicBool,
    ) -> Self {
        Self {
            walker,
            remaining: max_nodes as usize,
            truncated: false,
            cancel,
            depth,
        }
    }

    pub(super) fn node(
        &mut self,
        element: &IUIAutomationElement,
        path: Vec<usize>,
        level: u32,
    ) -> CuaResult<Node> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(CuaError::Cancelled);
        }
        if self.remaining == 0 {
            self.truncated = true;
        }
        self.remaining = self.remaining.saturating_sub(1);

        let patterns = Patterns::read(element);
        let password = unsafe { element.CurrentIsPassword() }
            .map(|flag| flag.as_bool())
            .unwrap_or(false);
        let role = unsafe { element.CurrentControlType() }
            .map(|id| control_type_name(id.0))
            .unwrap_or_else(|_| "UIA_Unknown".to_string());
        let mut node = Node {
            path: path.clone(),
            role,
            subrole: None,
            title: truncate(text(unsafe { element.CurrentName() })),
            description: truncate(text(unsafe { element.CurrentHelpText() })),
            value: if password {
                None
            } else {
                truncate(patterns.value_text())
            },
            identifier: truncate(text(unsafe { element.CurrentAutomationId() })),
            enabled: unsafe { element.CurrentIsEnabled() }
                .ok()
                .map(|flag| flag.as_bool()),
            focused: unsafe { element.CurrentHasKeyboardFocus() }
                .ok()
                .map(|flag| flag.as_bool()),
            actions: patterns.action_names(),
            bounds: element_bounds(element),
            children: Vec::new(),
        };
        if level >= self.depth {
            return Ok(node);
        }
        let mut child = unsafe { self.walker.GetFirstChildElement(element) }.ok();
        let mut index = 0;
        while let Some(current) = child {
            if self.remaining == 0 {
                self.truncated = true;
                break;
            }
            if self.cancel.load(Ordering::Relaxed) {
                return Err(CuaError::Cancelled);
            }
            let mut child_path = path.clone();
            child_path.push(index);
            node.children
                .push(self.node(&current, child_path, level + 1)?);
            index += 1;
            child = unsafe { self.walker.GetNextSiblingElement(&current) }.ok();
        }
        Ok(node)
    }
}

pub(super) fn resolve(
    walker: &IUIAutomationTreeWalker,
    root: &IUIAutomationElement,
    path: &[usize],
    expectation: Option<&Expectation>,
) -> CuaResult<IUIAutomationElement> {
    let mut element = root.clone();
    for index in path {
        let mut current = unsafe { walker.GetFirstChildElement(&element) }.ok();
        for _ in 0..*index {
            let Some(node) = current else {
                break;
            };
            current = unsafe { walker.GetNextSiblingElement(&node) }.ok();
        }
        element = current.ok_or_else(|| {
            CuaError::ElementChanged(format!(
                "element path {path:?} no longer exists at child {index}"
            ))
        })?;
    }
    if let Some(expectation) = expectation {
        check_expectation(&element, expectation)?;
    }
    Ok(element)
}

fn check_expectation(element: &IUIAutomationElement, expectation: &Expectation) -> CuaResult<()> {
    let role = unsafe { element.CurrentControlType() }
        .map(|id| control_type_name(id.0))
        .unwrap_or_else(|_| "UIA_Unknown".to_string());
    if role != expectation.role {
        return Err(CuaError::ElementChanged(format!(
            "expected {}, found {role} at {:?}",
            expectation.role, expectation.path
        )));
    }
    if let Some(identifier) = &expectation.identifier {
        let live = text(unsafe { element.CurrentAutomationId() });
        if live.as_deref() != Some(identifier.as_str()) {
            return Err(CuaError::ElementChanged(format!(
                "identifier changed from {identifier:?} to {live:?}"
            )));
        }
    }
    if let Some(title) = &expectation.title {
        let live = text(unsafe { element.CurrentName() });
        if live.as_deref() != Some(title.as_str()) {
            return Err(CuaError::ElementChanged(format!(
                "name changed from {title:?} to {live:?}"
            )));
        }
    }
    Ok(())
}

fn text(result: windows::core::Result<BSTR>) -> Option<String> {
    result
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

fn truncate(value: Option<String>) -> Option<String> {
    value.map(|value| {
        if value.chars().count() > 300 {
            let mut truncated: String = value.chars().take(300).collect();
            truncated.push('…');
            truncated
        } else {
            value
        }
    })
}
