//! The onboarding screens: signing in with Google, then the new account's
//! first sync until its inbox is ready. Their state is in
//! [`onboarding`](crate::onboarding).

use gpui::{AnyElement, App, FontWeight, IntoElement, ParentElement as _, Styled as _, div, px};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::progress::Progress;
use gpui_component::{ActiveTheme as _, v_flex};

use crate::bridge::AppModel;
use crate::onboarding::{Setup, SignIn};
use crate::typography::{TextRole, TypographyExt as _};

/// Width of the onboarding screens' text.
const TEXT_WIDTH: gpui::Pixels = px(440.);

/// The sign-in screen.
pub fn sign_in_screen(sign_in: &SignIn, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = |text: String| {
        div()
            .max_w(TEXT_WIDTH)
            .text_center()
            .text_role(TextRole::Body)
            .text_color(theme.muted_foreground)
            .child(text)
    };

    let screen = screen().child(
        div()
            .text_role(TextRole::Title)
            .child("Welcome to Openmail"),
    );
    match sign_in {
        SignIn::Ready { notice } => screen
            .child(muted(
                "Sign in with your Google account to read and send your Gmail.".to_owned(),
            ))
            .children(notice.as_ref().map(|notice| {
                let color = if notice.is_error() {
                    theme.danger
                } else {
                    theme.foreground
                };
                div()
                    .max_w(TEXT_WIDTH)
                    .text_center()
                    .text_role(TextRole::Secondary)
                    .text_color(color)
                    .child(notice.text())
            }))
            .child(
                Button::new("sign-in")
                    .primary()
                    .label("Sign in with Google")
                    .on_click(|_, _, cx| {
                        AppModel::global(cx).update(cx, AppModel::sign_in);
                    }),
            ),
        SignIn::InBrowser => screen
            .child(muted(
                "Continue in your browser: finish signing in on the Google page that opened, \
                 then come back here."
                    .to_owned(),
            ))
            .child(Progress::new("sign-in-progress").loading(true).w(px(240.)))
            .child(
                Button::new("cancel-sign-in")
                    .outline()
                    .label("Cancel")
                    .on_click(|_, _, cx| {
                        AppModel::global(cx).update(cx, |model, _| model.cancel_sign_in());
                    }),
            ),
        SignIn::Unconfigured { help } => screen
            .child(
                div()
                    .text_role(TextRole::Heading)
                    .child("Openmail needs a Google OAuth client"),
            )
            .child(muted(help.clone())),
    }
    .into_any_element()
}

/// The setup screen, while `account`'s first sync stores its inbox.
pub fn setup_screen(account: &str, setup: &Setup, cx: &App) -> AnyElement {
    let theme = cx.theme();
    screen()
        .child(
            div()
                .text_role(TextRole::Secondary)
                .text_color(theme.muted_foreground)
                .font_weight(FontWeight::SEMIBOLD)
                .child(account.to_owned()),
        )
        .child(div().text_role(TextRole::Title).child(setup.title.clone()))
        .children(
            (!setup.stopped).then(|| Progress::new("setup-progress").loading(true).w(px(240.))),
        )
        .children(setup.detail.clone().map(|detail| {
            div()
                .max_w(TEXT_WIDTH)
                .text_center()
                .text_role(TextRole::Body)
                .text_color(if setup.stopped {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(detail)
        }))
        .into_any_element()
}

/// A centered column filling the window below the title bar.
fn screen() -> gpui::Div {
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .gap_4()
        .px_8()
}
