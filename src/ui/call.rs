//! The call interface: the dialog of an incoming call, and the bar of a call
//! being placed or running, which grows a stage for the other side's video
//! while there is a picture to show. A video call's picture can pop out into
//! a borderless window of its own, and the bar then stays slim.

use std::time::Duration;

use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, Rect, Sense, Stroke, Vec2, pos2,
    vec2,
};

use crate::app::App;
use crate::model::{Action, CallMedia, CallPhase, CallView};
use crate::theme::{self, Icon};

const BUTTON: f32 = 48.0;

/// What a call's line says: who is calling, or how far along it is.
fn status(call: &CallView, now_ms: i64) -> String {
    match (call.phase, call.incoming) {
        (CallPhase::Ringing, true) if call.media == CallMedia::Video => {
            "Incoming video call".to_owned()
        }
        (CallPhase::Ringing, true) => "Incoming voice call".to_owned(),
        (CallPhase::Ringing, false) => "Calling…".to_owned(),
        (CallPhase::Connecting, _) => "Connecting…".to_owned(),
        (CallPhase::Connected, _) => timer(call.since, now_ms),
        (CallPhase::Ended, _) => "Call ended".to_owned(),
    }
}

/// Elapsed time of a connected call such as "1:05", or "1:02:03" past an hour.
fn timer(since: i64, now_ms: i64) -> String {
    let seconds = ((now_ms - since) / 1000).max(0) as u32;
    match seconds / 3600 {
        0 => crate::util::duration(seconds),
        hours => format!("{hours}:{:02}:{:02}", seconds / 60 % 60, seconds % 60),
    }
}

fn now_ms() -> i64 {
    crate::util::now() * 1000
}

/// A round button with an icon, drawn in a fixed square so a row of them
/// lines up.
fn round_button(
    ui: &mut egui::Ui,
    icon: Icon,
    size: f32,
    fill: Color32,
    color: Color32,
    label: &str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    theme::reveal_focus(&response);
    theme::focus_outline(ui, response.id, rect, size / 2.0);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label)
    });
    if ui.is_rect_visible(rect) {
        let fill = if response.hovered() {
            fill.gamma_multiply(0.85)
        } else {
            fill
        };
        ui.painter().circle_filled(rect.center(), size / 2.0, fill);
        theme::paint_icon(ui, icon, rect, size * 0.45, color);
    }
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(label)
}

/// The dialog of a call ringing here, with Accept and Decline. It floats over
/// the window instead of blocking it: the reader can finish a sentence first.
pub fn incoming(app: &mut App, ctx: &egui::Context) {
    let Some(call) = app
        .call
        .clone()
        .filter(|call| call.incoming && call.phase == CallPhase::Ringing)
    else {
        return;
    };
    let palette = app.palette;
    let picture = app.avatar(&call.chat);
    let width = 300.0;
    egui::Area::new(egui::Id::new("incoming-call"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 48.0))
        .show(ctx, |ui| {
            Frame::new()
                .fill(palette.overlay)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(theme::RADIUS + 4))
                .inner_margin(Margin::same(22))
                .shadow(palette.modal_shadow())
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.vertical_centered(|ui| {
                        super::widgets::avatar(
                            ui,
                            &palette,
                            &call.name,
                            &call.chat,
                            64.0,
                            picture.as_deref(),
                        );
                        ui.add_space(8.0);
                        super::widgets::rich_text(
                            ui,
                            &call.name,
                            theme::semibold(17.0),
                            palette.text,
                        );
                        super::widgets::rich_text(
                            ui,
                            &status(&call, now_ms()),
                            theme::regular(13.0),
                            palette.secondary,
                        );
                    });
                    ui.add_space(16.0);
                    let gap = 64.0;
                    ui.allocate_ui_with_layout(
                        vec2(width, BUTTON),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.add_space((width - 2.0 * BUTTON - gap) / 2.0);
                            if round_button(
                                ui,
                                Icon::PhoneOff,
                                BUTTON,
                                palette.danger,
                                Color32::WHITE,
                                "Decline",
                            )
                            .clicked()
                            {
                                app.actions.push(Action::RejectCall(call.id));
                            }
                            ui.add_space(gap);
                            if round_button(
                                ui,
                                Icon::Phone,
                                BUTTON,
                                palette.accent,
                                palette.on_accent,
                                "Accept",
                            )
                            .clicked()
                            {
                                app.actions.push(Action::AcceptCall(call.id));
                            }
                        },
                    );
                });
        });
}

/// The call bar's camera button: its icon, its label, and whether it is lit
/// (our camera is off in a video call). A voice call's button asks the other
/// side to switch to video, which needs a camera to send: Linux only for now.
fn camera_control(call: &CallView) -> Option<(Icon, &'static str, bool)> {
    if call.phase != CallPhase::Connected {
        return None;
    }
    Some(match (call.media, call.camera) {
        (CallMedia::Voice, _) if !cfg!(target_os = "linux") => return None,
        (CallMedia::Voice, true) => (Icon::Video, "Cancel switching to video", false),
        (CallMedia::Voice, false) => (Icon::Video, "Switch to video", false),
        (CallMedia::Video, true) => (Icon::Video, "Turn camera off", false),
        (CallMedia::Video, false) => (Icon::VideoOff, "Turn camera on", true),
    })
}

/// The size of a `picture` shown whole within `room`.
fn fit(picture: Vec2, room: Vec2) -> Vec2 {
    if picture.x <= 0.0 || picture.y <= 0.0 {
        return Vec2::ZERO;
    }
    picture * (room.x / picture.x).min(room.y / picture.y)
}

/// The bar above the window while a call is being placed or runs. Once the
/// other side's video has a picture, the bar shows it below its controls;
/// until then, and in a voice call, it is the bar alone.
pub fn bar(app: &mut App, ui: &mut egui::Ui) {
    let call = app
        .call
        .clone()
        .filter(|call| !(call.incoming && call.phase == CallPhase::Ringing));
    // Called without a call too, so the last call's texture goes.
    let picture = app.call_screen.show(
        ui.ctx(),
        call.as_ref()
            .filter(|call| call.phase != CallPhase::Ended)
            .and_then(|call| call.video.as_ref()),
    );
    let Some(call) = call else {
        return;
    };
    app.request_devices();
    let palette = app.palette;
    let stage_height = (ui.ctx().content_rect().height() * 0.45).clamp(120.0, 540.0);
    if call.phase == CallPhase::Connected {
        // The timer counts without any event to redraw it.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
    let mut popped = None;
    let shown = egui::Panel::top("call-bar")
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(palette.accent.gamma_multiply(0.18))
                .inner_margin(Margin::symmetric(14, 6)),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(BUTTON * 0.75);
                let icon = match call.media {
                    CallMedia::Video => Icon::Video,
                    CallMedia::Voice => Icon::Phone,
                };
                theme::icon(ui, icon, 18.0, palette.accent);
                ui.add_space(4.0);
                let small = BUTTON * 0.75;
                let camera_button = camera_control(&call);
                let pop_button = call.media == CallMedia::Video;
                let buttons = 3.0
                    + f32::from(u8::from(camera_button.is_some()))
                    + f32::from(u8::from(pop_button));
                let name_width = (ui.available_width() - buttons * (small + 8.0)).max(60.0);
                ui.allocate_ui_with_layout(
                    vec2(name_width, BUTTON * 0.75),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.set_max_width(name_width);
                        super::widgets::rich_text(
                            ui,
                            &call.name,
                            theme::semibold(14.0),
                            palette.text,
                        );
                        ui.add_space(8.0);
                        super::widgets::rich_text(
                            ui,
                            &status(&call, now_ms()),
                            theme::regular(13.0),
                            palette.secondary,
                        );
                    },
                );
                let mut devices_button = None;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if round_button(
                        ui,
                        Icon::PhoneOff,
                        small,
                        palette.danger,
                        Color32::WHITE,
                        "Hang up",
                    )
                    .clicked()
                    {
                        app.actions.push(Action::HangUp(call.id));
                    }
                    let (icon, label, fill, color) = if call.muted {
                        (Icon::MicOff, "Unmute", palette.text, palette.panel)
                    } else {
                        (Icon::Mic, "Mute", palette.outline, palette.text)
                    };
                    if round_button(ui, icon, small, fill, color, label).clicked() {
                        app.actions.push(Action::SetCallMuted(call.id, !call.muted));
                    }
                    if let Some((icon, label, lit)) = camera_button {
                        let (fill, color) = if lit {
                            (palette.text, palette.panel)
                        } else {
                            (palette.outline, palette.text)
                        };
                        if round_button(ui, icon, small, fill, color, label).clicked() {
                            app.actions
                                .push(Action::SetCallCamera(call.id, !call.camera));
                        }
                    }
                    if pop_button {
                        let (icon, label) = if app.call_popped {
                            (Icon::Minimize, "Show video in the call bar")
                        } else {
                            (Icon::ExternalLink, "Pop out video")
                        };
                        if round_button(ui, icon, small, palette.outline, palette.text, label)
                            .clicked()
                        {
                            app.actions.push(Action::PopOutCall(!app.call_popped));
                        }
                    }
                    devices_button = Some(round_button(
                        ui,
                        Icon::Settings,
                        small,
                        palette.outline,
                        palette.text,
                        "Devices",
                    ));
                });
                if let Some(button) = devices_button {
                    device_menu(app, ui, &button, call.media == CallMedia::Video);
                }
            });
            let mine = app
                .self_view
                .show(ui.ctx(), app.self_preview.as_ref().filter(|_| call.camera));
            if call.camera {
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
            if app.call_popped {
                // The video has a window of its own.
                popped = Some((picture.clone(), mine));
                return None;
            }
            let Some(picture) = &picture else {
                if let Some(mine) = &mine {
                    ui.add_space(6.0);
                    let width = 180.0_f32.min(ui.available_width());
                    let height = width * mine.size_vec2().y / mine.size_vec2().x.max(1.0);
                    let (stage, _) =
                        ui.allocate_exact_size(vec2(width, height.max(1.0)), Sense::hover());
                    paint_self(ui, mine, stage);
                    ui.add_space(4.0);
                }
                return None;
            };
            ui.add_space(6.0);
            let width = ui.available_width();
            let (stage, _) = ui.allocate_exact_size(vec2(width, stage_height), Sense::hover());
            let (shown, self_rect) = split_stage(
                stage,
                picture.size_vec2(),
                mine.as_ref().map(egui::TextureHandle::size_vec2),
            );
            egui::Image::new((picture.id(), shown.size()))
                .corner_radius(CornerRadius::same(theme::RADIUS))
                .paint_at(ui, shown);
            if let (Some(mine), Some(self_rect)) = (&mine, self_rect) {
                paint_self(ui, mine, self_rect);
            }
            ui.add_space(4.0);
            Some(shown)
        });
    ui.ctx().data_mut(|data| {
        data.insert_temp(bar_id(), shown.response.rect);
        match shown.inner {
            Some(stage) => {
                data.insert_temp(video_id(), stage);
            }
            None => data.remove::<Rect>(video_id()),
        }
    });
    if let Some((theirs, mine)) = popped {
        popout(app, ui.ctx(), &call, theirs.as_ref(), mine.as_ref());
    }
}

/// The popped-out video window's size when it opens: a phone held upright.
const POPOUT_SIZE: Vec2 = vec2(420.0, 640.0);
/// How far in from the window's edge a drag resizes it instead of moving it.
const RESIZE_BAND: f32 = 8.0;

pub(crate) fn popout_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("call-video-window")
}

/// The call's video in a borderless window of its own. Their picture covers
/// the whole window and ours sits small in its corner. A drag anywhere moves
/// the window, its edges resize it, and the controls show while the pointer
/// is over it. Closing it puts the video back in the call bar.
fn popout(
    app: &mut App,
    ctx: &egui::Context,
    call: &CallView,
    theirs: Option<&egui::TextureHandle>,
    mine: Option<&egui::TextureHandle>,
) {
    let builder = egui::ViewportBuilder::default()
        .with_title(format!("{} - ZapFast", call.name))
        .with_app_id("zapfast-call")
        .with_decorations(false)
        .with_resizable(true)
        .with_inner_size(POPOUT_SIZE)
        .with_min_inner_size([160.0, 120.0]);
    ctx.show_viewport_immediate(popout_id(), builder, |ui, _class| {
        if ui.input(|input| input.viewport().close_requested()) {
            app.actions.push(Action::PopOutCall(false));
        }
        let window = ui.max_rect();
        let painter = ui.painter();
        painter.rect_filled(window, CornerRadius::ZERO, Color32::BLACK);
        match theirs {
            Some(picture) => {
                egui::Image::new((picture.id(), window.size()))
                    .uv(cover_uv(picture.size_vec2(), window.size()))
                    .paint_at(ui, window);
            }
            None => {
                let galley = painter.layout(
                    format!("{}\n{}", call.name, status(call, now_ms())),
                    theme::medium(15.0),
                    Color32::WHITE,
                    window.width() - 24.0,
                );
                painter.galley(
                    window.center() - galley.size() / 2.0,
                    galley,
                    Color32::WHITE,
                );
            }
        }
        if let Some(mine) = mine {
            paint_self(ui, mine, self_corner(window, mine.size_vec2()));
        }
        let body = ui.interact(window, ui.id().with("move"), Sense::click_and_drag());
        if body.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if body.double_clicked() {
            app.actions.push(Action::PopOutCall(false));
        }
        resize_edges(ui, window);
        if ui.rect_contains_pointer(window) {
            popout_controls(app, ui, call, window);
        }
    });
}

/// The part of a picture that covers a window of `room` without stretching:
/// the middle, with what does not fit cut from both sides.
fn cover_uv(picture: Vec2, room: Vec2) -> Rect {
    if picture.x <= 0.0 || picture.y <= 0.0 || room.x <= 0.0 || room.y <= 0.0 {
        return Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    }
    let scale = (room.x / picture.x).max(room.y / picture.y);
    let shown = room / (picture * scale);
    Rect::from_center_size(pos2(0.5, 0.5), shown)
}

/// Where our picture sits in the popped-out window: the bottom right corner,
/// a quarter of the window's width and no wider than 160 points.
fn self_corner(window: Rect, mine: Vec2) -> Rect {
    const MARGIN: f32 = 12.0;
    let width = (window.width() * 0.25).min(160.0);
    let height = width * mine.y / mine.x.max(1.0);
    Rect::from_min_size(
        window.right_bottom() - vec2(width + MARGIN, height + MARGIN),
        vec2(width, height),
    )
}

/// Bands along the window's edges and corners that resize it.
fn resize_edges(ui: &mut egui::Ui, window: Rect) {
    use egui::{CursorIcon as Cursor, ResizeDirection as Direction};
    let band = RESIZE_BAND;
    let (left, right, top, bottom) = (window.left(), window.right(), window.top(), window.bottom());
    let edges = [
        (
            Rect::from_min_max(pos2(left + band, top), pos2(right - band, top + band)),
            Direction::North,
            Cursor::ResizeNorth,
        ),
        (
            Rect::from_min_max(pos2(left + band, bottom - band), pos2(right - band, bottom)),
            Direction::South,
            Cursor::ResizeSouth,
        ),
        (
            Rect::from_min_max(pos2(left, top + band), pos2(left + band, bottom - band)),
            Direction::West,
            Cursor::ResizeWest,
        ),
        (
            Rect::from_min_max(pos2(right - band, top + band), pos2(right, bottom - band)),
            Direction::East,
            Cursor::ResizeEast,
        ),
        (
            Rect::from_min_size(pos2(left, top), Vec2::splat(band)),
            Direction::NorthWest,
            Cursor::ResizeNorthWest,
        ),
        (
            Rect::from_min_size(pos2(right - band, top), Vec2::splat(band)),
            Direction::NorthEast,
            Cursor::ResizeNorthEast,
        ),
        (
            Rect::from_min_size(pos2(left, bottom - band), Vec2::splat(band)),
            Direction::SouthWest,
            Cursor::ResizeSouthWest,
        ),
        (
            Rect::from_min_size(pos2(right - band, bottom - band), Vec2::splat(band)),
            Direction::SouthEast,
            Cursor::ResizeSouthEast,
        ),
    ];
    for (index, (rect, direction, cursor)) in edges.into_iter().enumerate() {
        let response = ui
            .interact(rect, ui.id().with(("resize", index)), Sense::drag())
            .on_hover_cursor(cursor);
        if response.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
        }
    }
}

/// Mute, camera, back to the call bar, and hang up, along the bottom of the
/// popped-out window while the pointer is over it.
fn popout_controls(app: &mut App, ui: &mut egui::Ui, call: &CallView, window: Rect) {
    const SIZE: f32 = 40.0;
    const GAP: f32 = 10.0;
    let palette = app.palette;
    let camera = camera_control(call);
    let count = 3.0 + f32::from(u8::from(camera.is_some()));
    let width = count * SIZE + (count - 1.0) * GAP;
    let row = Rect::from_center_size(
        pos2(window.center().x, window.bottom() - 16.0 - SIZE / 2.0),
        vec2(width, SIZE),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(row)
            .layout(Layout::left_to_right(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            let (icon, label, fill, color) = if call.muted {
                (Icon::MicOff, "Unmute", palette.text, palette.panel)
            } else {
                (Icon::Mic, "Mute", palette.outline, palette.text)
            };
            if round_button(ui, icon, SIZE, fill, color, label).clicked() {
                app.actions.push(Action::SetCallMuted(call.id, !call.muted));
            }
            if let Some((icon, label, lit)) = camera {
                let (fill, color) = if lit {
                    (palette.text, palette.panel)
                } else {
                    (palette.outline, palette.text)
                };
                if round_button(ui, icon, SIZE, fill, color, label).clicked() {
                    app.actions
                        .push(Action::SetCallCamera(call.id, !call.camera));
                }
            }
            if round_button(
                ui,
                Icon::Minimize,
                SIZE,
                palette.outline,
                palette.text,
                "Show video in the call bar",
            )
            .clicked()
            {
                app.actions.push(Action::PopOutCall(false));
            }
            if round_button(
                ui,
                Icon::PhoneOff,
                SIZE,
                palette.danger,
                Color32::WHITE,
                "Hang up",
            )
            .clicked()
            {
                app.actions.push(Action::HangUp(call.id));
            }
        },
    );
}

/// Where the other side's picture and ours go on the stage. Ours sits against
/// the left edge, at most 180 points wide; theirs fits whole in the rest, so
/// ours never covers them.
fn split_stage(stage: Rect, theirs: Vec2, mine: Option<Vec2>) -> (Rect, Option<Rect>) {
    const GAP: f32 = 8.0;
    let Some(mine) = mine else {
        let size = fit(theirs, stage.size());
        return (Rect::from_center_size(stage.center(), size), None);
    };
    let aspect = mine.x / mine.y.max(1.0);
    let mut height = stage.height();
    let mut width = height * aspect;
    let widest = (stage.width() * 0.3).min(180.0);
    if width > widest {
        width = widest;
        height = width / aspect.max(0.01);
    }
    let own = Rect::from_min_size(
        pos2(stage.left(), stage.center().y - height / 2.0),
        vec2(width, height),
    );
    let rest = Rect::from_min_max(pos2(own.right() + GAP, stage.top()), stage.max);
    let size = fit(theirs, rest.size());
    (Rect::from_center_size(rest.center(), size), Some(own))
}

fn paint_self(ui: &egui::Ui, texture: &egui::TextureHandle, rect: Rect) {
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), Color32::BLACK);
    egui::Image::new((texture.id(), rect.size()))
        .corner_radius(CornerRadius::same(8))
        .paint_at(ui, rect);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(8),
        Stroke::new(2.0, Color32::WHITE),
        egui::StrokeKind::Inside,
    );
}

enum DevicePart {
    Microphone,
    Speaker,
    Camera,
}

/// Microphone, speaker, and camera for the call that is up.
///
/// Each kind is a submenu, so a long list cannot push the others off the
/// screen. A combo box nested here would open a second popup, and the click
/// that shows it is an outside click on this menu, which closes it.
fn device_menu(app: &mut App, _ui: &mut egui::Ui, button: &egui::Response, video: bool) {
    let palette = app.palette;
    let microphones = app.devices.microphones.clone();
    let speakers = app.devices.speakers.clone();
    let cameras = app.devices.cameras.clone();
    egui::Popup::menu(button)
        .width(280.0)
        .frame(super::widgets::menu_frame(&palette))
        .show(|ui| {
            device_choices(ui, app, "Microphone", &microphones, DevicePart::Microphone);
            device_choices(ui, app, "Speaker", &speakers, DevicePart::Speaker);
            if video || !cameras.is_empty() {
                device_choices(ui, app, "Camera", &cameras, DevicePart::Camera);
            }
        });
}

fn device_choices(
    ui: &mut egui::Ui,
    app: &mut App,
    title: &str,
    names: &[String],
    part: DevicePart,
) {
    let current = match part {
        DevicePart::Microphone => app.settings.microphone.clone(),
        DevicePart::Speaker => app.settings.speaker.clone(),
        DevicePart::Camera => app.settings.camera.clone(),
    };
    let icon = match part {
        DevicePart::Microphone => Icon::Mic,
        DevicePart::Speaker => Icon::Volume2,
        DevicePart::Camera => Icon::Video,
    };
    let palette = app.palette;
    let shown = if current.is_empty() {
        "System default"
    } else {
        current.as_str()
    };
    let label = format!("{title}: {shown}");
    let mut choices = vec![("System default".to_owned(), String::new())];
    choices.extend(names.iter().cloned().map(|name| (name.clone(), name)));
    if !current.is_empty() && !names.iter().any(|name| name == &current) {
        choices.push((current.clone(), current.clone()));
    }
    super::widgets::submenu(ui, &palette, icon, &label, |ui| {
        ui.set_min_width(260.0);
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .show(ui, |ui| device_rows(ui, app, choices, &current, &part));
    });
}

fn device_rows(
    ui: &mut egui::Ui,
    app: &mut App,
    choices: Vec<(String, String)>,
    current: &str,
    part: &DevicePart,
) {
    let palette = app.palette;
    for (label, name) in choices {
        let icon = (name == current).then_some(Icon::Check);
        if super::widgets::menu_item(ui, &palette, icon, &label) {
            let mut microphone = app.settings.microphone.clone();
            let mut speaker = app.settings.speaker.clone();
            let mut camera = app.settings.camera.clone();
            match part {
                DevicePart::Microphone => microphone = name,
                DevicePart::Speaker => speaker = name,
                DevicePart::Camera => camera = name,
            }
            app.actions.push(Action::SetCallDevices {
                microphone,
                speaker,
                camera,
            });
        }
    }
}

/// Where the in-call bar was laid out, for layout tests.
pub(crate) fn bar_id() -> egui::Id {
    egui::Id::new("call-bar-rect")
}

/// Where the other side's video was drawn, for layout tests.
pub(crate) fn video_id() -> egui::Id {
    egui::Id::new("call-video-rect")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timer_counts_from_the_connection() {
        assert_eq!(timer(1_000, 1_000), "0:00");
        assert_eq!(timer(0, 65_900), "1:05");
        assert_eq!(timer(0, 3_723_000), "1:02:03");
        // A clock that stepped back does not show a negative time.
        assert_eq!(timer(5_000, 1_000), "0:00");
    }

    #[test]
    fn the_picture_fits_its_stage_whole() {
        assert_eq!(
            fit(vec2(1280.0, 720.0), vec2(640.0, 540.0)),
            vec2(640.0, 360.0)
        );
        assert_eq!(
            fit(vec2(720.0, 1280.0), vec2(800.0, 320.0)),
            vec2(180.0, 320.0)
        );
        assert_eq!(fit(vec2(0.0, 0.0), vec2(800.0, 320.0)), Vec2::ZERO);
    }

    #[test]
    fn a_ringing_video_call_says_so() {
        let mut call = CallView {
            id: crate::model::CallId(1),
            chat: "1@s.whatsapp.net".into(),
            name: "Ada".into(),
            incoming: true,
            media: CallMedia::Video,
            phase: CallPhase::Ringing,
            since: 0,
            muted: false,
            camera: false,
            video: None,
        };
        assert_eq!(status(&call, 0), "Incoming video call");
        call.media = CallMedia::Voice;
        assert_eq!(status(&call, 0), "Incoming voice call");
    }

    #[test]
    fn a_popped_out_picture_covers_its_window_from_the_middle() {
        // A wide picture in a tall window: its sides are cut evenly.
        let uv = cover_uv(vec2(1280.0, 720.0), vec2(360.0, 640.0));
        assert!((uv.height() - 1.0).abs() < 1e-4);
        assert!((uv.width() - (360.0 / 640.0) / (1280.0 / 720.0)).abs() < 1e-4);
        assert!((uv.center().x - 0.5).abs() < 1e-4);
        // The same shape fills it whole.
        let whole = cover_uv(vec2(360.0, 640.0), vec2(720.0, 1280.0));
        assert!((whole.size() - Vec2::splat(1.0)).length() < 1e-4);
        assert_eq!(
            cover_uv(Vec2::ZERO, vec2(10.0, 10.0)).size(),
            Vec2::splat(1.0)
        );
    }

    #[test]
    fn our_picture_sits_in_the_popped_out_windows_corner() {
        let window = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let mine = self_corner(window, vec2(240.0, 320.0));
        assert_eq!(mine.width(), 160.0, "no wider than 160 points");
        assert!((mine.height() - 160.0 * 320.0 / 240.0).abs() < 1e-3);
        assert!(window.contains_rect(mine));
        assert!(mine.right() > 780.0 && mine.bottom() > 580.0);
        let small = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 300.0));
        assert_eq!(self_corner(small, vec2(240.0, 320.0)).width(), 50.0);
    }

    #[test]
    fn a_connected_voice_call_offers_to_switch_to_video() {
        let label = |call: &CallView| camera_control(call).map(|(_, label, _)| label);
        let mut call = CallView {
            id: crate::model::CallId(1),
            chat: "1@s.whatsapp.net".into(),
            name: "Ada".into(),
            incoming: false,
            media: CallMedia::Voice,
            phase: CallPhase::Ringing,
            since: 0,
            muted: false,
            camera: false,
            video: None,
        };
        assert_eq!(label(&call), None, "not before the call connects");
        call.phase = CallPhase::Connected;
        if cfg!(target_os = "linux") {
            assert_eq!(label(&call), Some("Switch to video"));
            call.camera = true;
            assert_eq!(label(&call), Some("Cancel switching to video"));
        } else {
            assert_eq!(label(&call), None, "no camera to send");
        }
        call.media = CallMedia::Video;
        call.camera = true;
        assert_eq!(label(&call), Some("Turn camera off"));
        call.camera = false;
        assert_eq!(label(&call), Some("Turn camera on"));
    }

    #[test]
    fn our_picture_sits_left_of_theirs_without_covering_it() {
        let stage = Rect::from_min_size(pos2(10.0, 20.0), vec2(800.0, 300.0));
        let (theirs, mine) = split_stage(stage, vec2(480.0, 640.0), Some(vec2(240.0, 320.0)));
        let mine = mine.expect("our picture");
        assert_eq!(mine.left(), stage.left());
        assert!(mine.width() <= 180.0);
        assert!(mine.height() <= stage.height());
        assert!(theirs.left() >= mine.right(), "{theirs:?} {mine:?}");
        assert!(stage.contains_rect(theirs));

        // A wide picture on a narrow stage still stays clear of ours.
        let narrow = Rect::from_min_size(pos2(0.0, 0.0), vec2(320.0, 240.0));
        let (theirs, mine) = split_stage(narrow, vec2(1280.0, 720.0), Some(vec2(240.0, 320.0)));
        let mine = mine.expect("our picture");
        assert!(theirs.left() >= mine.right());
        assert!(narrow.contains_rect(theirs));

        let (alone, none) = split_stage(stage, vec2(480.0, 640.0), None);
        assert!(none.is_none());
        assert_eq!(alone.center(), stage.center());
    }
}
