//! GPUI Kit controls observed and driven by the ordinary MCP bridge: the
//! application the server's end-to-end tests drive.

use gpui_kit::{
    Context, Div, Entity, FocusHandle, IntoElement, Render, Role, Stateful, StyleRefinement,
    WeakEntity, Window,
    component::{
        button::Button,
        checkbox::Checkbox,
        input::{Input, InputState},
        slider::{Slider, SliderState},
    },
    div,
    prelude::*,
    px, rgb,
};
use gpui_mcp::{AnnotationSource, Automation, BridgeHandle, NewMessage};

/// The window title and the application's accessible name.
pub const TITLE: &str = "GPUI Kit MCP Demo";

/// The volume slider's range and its value at launch.
const VOLUME_MAX: f32 = 10.0;
const VOLUME_START: f32 = 5.0;
const VOLUME_STEP: f32 = 1.0;

/// One region drawn as its own cached view, the way a workbench draws each of
/// its panels. Hovering its control notifies this view alone, so a frame report
/// taken across the hover should show this region rendering and its sibling
/// replaying from cache. Anything that renders both, such as a window refresh,
/// is visible in the report as the cause of the sibling's render.
pub struct ProbeRegion {
    id: &'static str,
    target: &'static str,
    label: &'static str,
}

impl Render for ProbeRegion {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().id(self.id).size_full().flex().items_center().child(
            div()
                .id(self.target)
                .px_4()
                .py_2()
                .rounded_md()
                .bg(rgb(0x22_28_33))
                .hover(|style| style.bg(rgb(0x39_42_53)))
                .child(self.label)
                .aria_label(self.label),
        )
    }
}

/// One keyboard-focusable field carrying the two shapes of visual change a
/// region crop has to survive: focus draws a one-pixel, high-contrast ring on
/// the field's own bounds, and hover repaints its whole interior a single step
/// lighter. A crop that is stale fails the first; a crop that refreshes on
/// layout but not on paint can still pass the second, so both are needed.
///
/// The focus treatment is a plain conditional border and deliberately not
/// GPUI's `focus_visible`, which additionally requires
/// `Window::last_input_was_keyboard()`. A programmatic focus dispatches no
/// platform input and a programmatic pointer move claims Mouse modality, so a
/// `focus_visible` ring here would never paint for a driving test that parks
/// the pointer first, and the region capture test would pass while measuring
/// nothing.
fn field(
    id: &'static str,
    label: &'static str,
    handle: &FocusHandle,
    focused: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .track_focus(handle)
        .w(px(240.0))
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(if focused { rgb(0x16_77_ff) } else { rgb(0x39_42_53) })
        .bg(rgb(0x0b_0e_14))
        .hover(|style| style.bg(rgb(0x12_16_1e)))
        .child(label)
        .role(Role::TextInput)
        .aria_label(label)
}

/// GPUI Kit's controls beside the plain elements the bridge's tests need,
/// retaining its bridge for the window's lifetime.
pub struct Demo {
    count: usize,
    locked: bool,
    agreed: bool,
    input: Entity<InputState>,
    /// GPUI Kit's own slider: it registers the Increment and Decrement
    /// accessibility actions and no key handler.
    volume: Entity<SliderState>,
    /// The volume last logged, so only a change writes a line.
    logged_volume: f32,
    search: FocusHandle,
    filter: FocusHandle,
    probes: [Entity<ProbeRegion>; 2],
    /// The agent's latest message, shown the way a chat box would.
    reply: Option<String>,
    /// How many annotations exist and who changed them last, mirrored from the bridge.
    annotations: (usize, Option<AnnotationSource>),
    /// Where the counter and the volume write their changes: the bridge's log,
    /// which `get_logs` returns. Tests attach their own observation and log
    /// nothing.
    automation: Option<Automation>,
    bridge: Option<BridgeHandle>,
}

impl Demo {
    /// Create the demo; tests attach isolated observation and pass no IPC bridge.
    pub fn new(window: &mut Window, cx: &mut Context<Self>, bridge: Option<BridgeHandle>) -> Self {
        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(VOLUME_MAX)
                .step(VOLUME_STEP)
                .default_value(VOLUME_START)
        });
        // Kit's slider emits no event when an accessibility action sets its
        // value, so the change is read from the state itself.
        cx.observe(&volume, |this, volume, cx| {
            let value = volume.read(cx).value().end();
            // the slider moves in whole steps: half a step tells a change from rounding
            if (value - this.logged_volume).abs() >= VOLUME_STEP / 2.0 {
                this.logged_volume = value;
                this.log(&format!("volume changed to {value}"));
            }
        })
        .detach();
        if let Some(bridge) = &bridge {
            Self::listen(bridge, cx.entity().downgrade());
        }
        Self {
            count: 0,
            locked: true,
            agreed: false,
            input: cx.new(|cx| InputState::new(window, cx).placeholder("Enter your name")),
            volume,
            logged_volume: VOLUME_START,
            search: cx.focus_handle(),
            filter: cx.focus_handle(),
            probes: [
                cx.new(|_| ProbeRegion {
                    id: "probe-left",
                    target: "probe-left-target",
                    label: "Left probe",
                }),
                cx.new(|_| ProbeRegion {
                    id: "probe-right",
                    target: "probe-right-target",
                    label: "Right probe",
                }),
            ],
            reply: None,
            annotations: (0, None),
            automation: bridge.as_ref().map(BridgeHandle::automation),
            bridge,
        }
    }

    /// Show the agent's messages and mirror its annotations, as a chat box would.
    #[expect(
        clippy::print_stderr,
        reason = "the demo reports a bridge it cannot listen to on stderr, as it does every start failure"
    )]
    fn listen(bridge: &BridgeHandle, demo: WeakEntity<Self>) {
        let registered = bridge
            .on_message({
                let demo = demo.clone();
                move |message, _, cx| {
                    let _ = demo.update(cx, |demo, cx| {
                        demo.reply = Some(message.text.clone());
                        cx.notify();
                    });
                }
            })
            .and_then(|()| {
                bridge.on_annotations(move |event, _, cx| {
                    let _ = demo.update(cx, |demo, cx| {
                        demo.annotations = (event.annotations.len(), Some(event.changed_by));
                        cx.notify();
                    });
                })
            });
        if let Err(error) = registered {
            eprintln!("could not register bridge callbacks: {error}");
        }
    }

    fn log(&self, message: &str) {
        if let Some(automation) = &self.automation {
            automation.log("info", message);
        }
    }

    fn increment(&mut self, cx: &mut Context<Self>) {
        self.count = self.count.saturating_add(1);
        self.log(&format!("counter changed to {}", self.count));
        cx.notify();
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.count = 0;
        self.log("counter reset");
        cx.notify();
    }

    /// Post a chat message for the connected agent, as an in-app chat box would.
    fn ask_agent(&mut self, cx: &mut Context<Self>) {
        let message = NewMessage::text(format!("Hello from the demo; the count is {}", self.count));
        if let Some(bridge) = &self.bridge
            && let Err(error) = bridge.post_message(message)
        {
            self.log(&error.message);
        }
        cx.notify();
    }

    fn toggle_lock(&mut self, cx: &mut Context<Self>) {
        self.locked = !self.locked;
        self.log(if self.locked {
            "the locked action refuses input"
        } else {
            "the locked action accepts input"
        });
        cx.notify();
    }

    /// A chat box in miniature: a send button, the agent's latest reply, and a
    /// mirror of the annotations the agent has placed.
    fn chat_row(&self, cx: &mut Context<Self>) -> Div {
        let (count, changed_by) = self.annotations;
        let reply = self.reply.clone().unwrap_or_else(|| "No reply yet".to_owned());
        let annotations = format!(
            "Annotations: {count}{}",
            match changed_by {
                Some(AnnotationSource::Agent) => " (agent)",
                Some(AnnotationSource::App) => " (app)",
                Some(AnnotationSource::Bridge) => " (expired)",
                None => "",
            }
        );
        div()
            .flex()
            .gap_3()
            .items_center()
            .child(
                Button::new("chat-send")
                    .label("Ask agent")
                    .on_click(cx.listener(|this, _, _, cx| this.ask_agent(cx))),
            )
            .child(div().id("chat-reply").child(reply.clone()).role(Role::Status).aria_label(reply))
            .child(
                div()
                    .id("annotation-count")
                    .child(annotations.clone())
                    .role(Role::Status)
                    .aria_label(annotations),
            )
    }

    /// A control that refuses input, and the toggle that gives it back.
    ///
    /// The locked control states its own disabled state with `aria_disabled`,
    /// which is the only thing the UI tree's `enabled` is derived from: a widget
    /// that merely withholds its click handler and paints itself grey — which
    /// this one also does — still reports `enabled: true`, so the tree would
    /// assert a falsehood rather than admit it does not know. The toggle exists
    /// so the attribute can be measured flipping on one node rather than read
    /// once, which is the only way to tell a carried value from a constant.
    fn lock_row(&self, cx: &mut Context<Self>) -> Div {
        let locked = self.locked;
        div()
            .flex()
            .gap_3()
            .child(
                div()
                    .id("locked-action")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(if locked { rgb(0x22_28_33) } else { rgb(0x16_77_ff) })
                    .text_color(if locked { rgb(0x6b_74_85) } else { rgb(0xe8_ee_f7) })
                    .child("Locked action")
                    .role(Role::Button)
                    .aria_label("Locked action")
                    .aria_disabled(locked)
                    .when(!locked, |this| {
                        this.cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| this.increment(cx)))
                    }),
            )
            .child(
                div()
                    .id("lock-toggle")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x39_42_53))
                    .cursor_pointer()
                    .child(if locked { "Unlock" } else { "Lock" })
                    .role(Role::Button)
                    .aria_label(if locked { "Unlock" } else { "Lock" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_lock(cx))),
            )
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = self.search.is_focused(window);
        let filtering = self.filter.is_focused(window);
        div()
            .id("kit-demo")
            .role(Role::Application)
            .aria_label(TITLE)
            .size_full()
            .flex()
            .flex_col()
            .p_6()
            .gap_3()
            .child(div().id("heading").child(TITLE))
            .child(div().id("count").child(format!("Count: {}", self.count)))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(field("search", "Search", &self.search, searching))
                    .child(field("filter", "Filter", &self.filter, filtering)),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        Button::new("increment")
                            .label("Increment")
                            .on_click(cx.listener(|this, _, _, cx| this.increment(cx))),
                    )
                    .child(
                        Button::new("reset")
                            .label("Reset")
                            .on_click(cx.listener(|this, _, _, cx| this.reset(cx))),
                    ),
            )
            .child(self.lock_row(cx))
            .child(self.chat_row(cx))
            .child(Checkbox::new("agree").label("Agree").checked(self.agreed).on_change(
                cx.listener(|this, checked, _, cx| {
                    this.agreed = *checked;
                    cx.notify();
                }),
            ))
            .child(Input::new(&self.input).id("name"))
            .child(div().id("volume-row").w_64().child(Slider::new(&self.volume)))
            .child(div().flex().gap_3().children(self.probes.iter().map(|probe| {
                probe.clone().cached(StyleRefinement::default().w(px(200.0)).h(px(40.0)))
            })))
    }
}

#[cfg(test)]
mod tests;
