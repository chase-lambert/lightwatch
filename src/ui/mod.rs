pub mod graph;
pub mod theme;
pub mod view;

use crate::model::HistoryConfig;

pub fn run_gui(config: HistoryConfig) -> iced::Result {
    let boot_config = config.clone();
    iced::application(
        move || view::boot(boot_config.clone()),
        view::update,
        view::view,
    )
    .title(view::title)
    .subscription(view::subscription)
    .theme(view::theme)
    .window_size((800.0, 900.0))
    .run()
}
