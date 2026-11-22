//! The document shell and the routes.

use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::{SsrMode, path};

use crate::header::Header;
use crate::pages::{DayPage, MonthPage};

/// The HTML document around the app (server only).
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1"/>
                <meta name="color-scheme" content="light dark"/>
                <title>"energybrain"</title>
                <link rel="stylesheet" href="/pkg/energybrain.css"/>
                <AutoReload options=options.clone()/>
                <HydrationScripts options/>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}

/// The app: the live header, then the page for the route.
///
/// Routes render with [`SsrMode::Async`]: the server waits for every resource and sends one
/// complete document. The data is local and quick, so streaming would only add markup.
#[component]
pub fn App() -> impl IntoView {
    view! {
        <Router>
            <Header/>
            <nav class="tabs" aria-label="View">
                <A href="/" exact=true>"Day"</A>
                <A href="/month">"Month"</A>
            </nav>
            <main>
                <Routes fallback=|| view! { <p class="empty">"Nothing here."</p> }>
                    <Route path=path!("/") view=DayPage ssr=SsrMode::Async/>
                    <Route path=path!("/month") view=MonthPage ssr=SsrMode::Async/>
                </Routes>
            </main>
        </Router>
    }
}
