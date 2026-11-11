//! What publik needs to know about this build, and the gate every publik app opens with
//! (publikhq.com/skills/setup.md, the module at publikhq.com/sdk/publik.rs.md, shared with the
//! other Craft apps as the `craft-publik` crate).
//!
//! At start-up `craft_publik::ui::Gate::new(app_info(..))` runs `craft_publik::gate::ensure_signed_in`
//! on a worker thread and `App::ui` draws it before anything of PdfCraft: Sign in with publik
//! (`POST /api/v1/installs` mints this computer's key; `GET /api/v1/wallet` says when the person
//! signed in), then once how the app pays for AI (Get a plan, Connect your plan, Use my own key;
//! the answer lives in publik's credential file). Usage counting (`POST /api/v1/telemetry`:
//! `app_opened`, then `active_session` every 30 minutes, with the install key) starts when the
//! gate passed. `PUBLIK_SELF_TEST=1` prints `publik self-test: ok pdfcraft <version>` instead.

/// The publik listing: publikhq.com/pdfcraft. Also the credential file's name.
pub const APP_SLUG: &str = "pdfcraft";
pub const APP_NAME: &str = "PdfCraft";
/// Summarize and Ask about this document call a model, so the gate asks how the app pays for AI.
pub const USES_AI: bool = true;
/// The publik paths and events this app calls (the module's contract; publik's scan reads them here).
pub const ENDPOINTS: craft_publik::Endpoints = craft_publik::Endpoints {
    installs: "/api/v1/installs",
    wallet: "/api/v1/wallet",
    telemetry: "/api/v1/telemetry",
    opened: "app_opened",
    active: "active_session",
};

/// The version publik counts and the listing shows: the release tag without its `v`, for example
/// `0.19.1-publik.8`. The build number comes from `packaging/PUBLIK_BUILD`, the file the bundle
/// version comes from.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| format!("{}-publik.{}", env!("CARGO_PKG_VERSION"), include_str!("../../../packaging/PUBLIK_BUILD").trim()))
}

/// What publik needs to know about this build. The app token is public by design (it can only
/// mint capped keys attributed to PdfCraft) and lives at the repository root, so source builds
/// carry it. AI settings (`ai.json`) go in `config_dir`, the preferences folder, next to `app.ron`.
pub fn app_info(config_dir: Option<std::path::PathBuf>) -> craft_publik::AppInfo {
    craft_publik::AppInfo {
        slug: APP_SLUG,
        name: APP_NAME,
        version: version(),
        app_token: include_str!("../../../publik-app-token.txt"),
        uses_ai: USES_AI,
        endpoints: ENDPOINTS,
        dialects: &["chat_completions"],
        config_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_names_itself_to_publik() {
        let info = app_info(None);
        assert_eq!(info.slug, "pdfcraft");
        assert!(info.uses_ai);
        assert_eq!(info.endpoints, craft_publik::Endpoints::DEFAULT);
        assert!(version().starts_with(env!("CARGO_PKG_VERSION")));
        assert!(version().contains("-publik."));
        assert!(!version().starts_with('v'));
    }
}
