//! One module per page.

mod day;
mod month;

pub use day::DayPage;
use leptos::prelude::*;
pub use month::MonthPage;

/// Bumps `tick` every minute while `live()` holds (today, this month), so a page whose
/// resource depends on `tick` refetches and keeps filling in.
pub fn refresh_every_minute(tick: RwSignal<u32>, live: impl Fn() -> bool + 'static) {
    crate::in_browser(move || {
        set_interval(
            move || {
                if live() {
                    tick.update(|n| *n += 1);
                }
            },
            std::time::Duration::from_secs(60),
        );
    });
}
