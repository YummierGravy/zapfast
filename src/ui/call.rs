//! The call interface: the dialog of an incoming call, and the bar of a call
//! being placed or running, which grows a stage for the other side's video
//! while there is a picture to show.

use std::time::Duration;

use egui::{
    Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, Rect, Sense, Stroke, Vec2, vec2,
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
    let palette = app.palette;
    let stage_height = (ui.ctx().content_rect().height() * 0.45).clamp(120.0, 540.0);
    if call.phase == CallPhase::Connected {
        // The timer counts without any event to redraw it.
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
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
                let name_width = (ui.available_width() - 2.0 * (BUTTON * 0.75 + 8.0)).max(60.0);
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
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let small = BUTTON * 0.75;
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
                });
            });
            let Some(picture) = &picture else {
                return None;
            };
            ui.add_space(6.0);
            let width = ui.available_width();
            let (stage, _) = ui.allocate_exact_size(vec2(width, stage_height), Sense::hover());
            let size = fit(picture.size_vec2(), stage.size());
            let shown = Rect::from_center_size(stage.center(), size);
            egui::Image::new((picture.id(), size))
                .corner_radius(CornerRadius::same(theme::RADIUS))
                .paint_at(ui, shown);
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
            video: None,
        };
        assert_eq!(status(&call, 0), "Incoming video call");
        call.media = CallMedia::Voice;
        assert_eq!(status(&call, 0), "Incoming voice call");
    }
}
