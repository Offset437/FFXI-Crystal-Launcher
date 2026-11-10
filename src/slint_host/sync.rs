//! What every page's view model shares: setting a property only when it changed, updating a list
//! model row by row, masking identities inside free text while privacy mode is on, and (in tests)
//! the real window on a headless software renderer.
use crate::App;
use slint::{Model, SharedString, VecModel};

/// Sets a property only when the new value differs from the shown one.
macro_rules! put {
    ($view:expr, $get:ident, $set:ident, $value:expr) => {{
        let value = $value;
        if $view.$get() != value {
            $view.$set(value);
        }
    }};
}
pub(super) use put;

/// Updates `model` to `rows` in place: only rows that differ are replaced.
pub(super) fn sync_model<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let old = model.row_count();
    for (i, row) in rows.iter().enumerate() {
        if i < old {
            if model.row_data(i).as_ref() != Some(row) {
                model.set_row_data(i, row.clone());
            }
        } else {
            model.push(row.clone());
        }
    }
    for i in (rows.len()..old).rev() {
        model.remove(i);
    }
}

pub(super) fn strings_model(model: &VecModel<SharedString>, items: impl IntoIterator<Item = String>) {
    sync_model(model, items.into_iter().map(SharedString::from).collect());
}

/// Masks whatever identifies someone inside free text while privacy mode is on. The names are gathered once, for every text of one sync.
pub(super) struct Private<'a> {
    names: Option<Vec<&'a str>>,
}

impl<'a> Private<'a> {
    pub(super) fn new(app: &'a App) -> Self {
        Self { names: crate::privacy::on().then(|| crate::privacy::identities(app)) }
    }

    /// `text`, masked where it names someone.
    pub(super) fn text(&self, text: &str) -> String {
        let Some(names) = &self.names else { return text.to_owned() };
        crate::privacy::split(text, names)
            .into_iter()
            .map(|(part, hide)| if hide { crate::privacy::mask(&part) } else { part })
            .collect()
    }
}

/// One text masked as `Private::text` does, for a sync that masks only one or two.
pub(super) fn private(app: &App, text: &str) -> String {
    Private::new(app).text(text)
}

#[cfg(test)]
pub(super) mod headless {
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType, Rgb565Pixel};
    use std::rc::Rc;

    struct Headless(Rc<MinimalSoftwareWindow>);
    impl slint::platform::Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
            Ok(self.0.clone())
        }
    }

    /// A `width` x `height` software window as this test thread's platform (one per thread:
    /// Slint takes a platform once). Components created afterwards draw into it.
    pub(in crate::slint_host) fn window(width: u32, height: u32) -> Rc<MinimalSoftwareWindow> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        window.set_size(slint::PhysicalSize::new(width, height));
        slint::platform::set_platform(Box::new(Headless(window.clone()))).unwrap();
        window
    }

    /// Draws one frame (which builds the shown items) and throws the pixels away.
    pub(in crate::slint_host) fn draw(window: &MinimalSoftwareWindow) {
        let size = window.size();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            let mut pixels = vec![Rgb565Pixel::default(); size.width as usize * size.height as usize];
            renderer.render(&mut pixels, size.width as usize);
        });
    }
}
