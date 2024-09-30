use core::fmt::{self, Display};
use std::sync::LazyLock;

use camino::Utf8Path;
use gethostname::gethostname;

static HOSTNAME: LazyLock<String> = LazyLock::new(|| {
    gethostname()
        .to_str()
        .map_or_else(String::new, String::from)
});

pub struct Hyperlink<H: Display, T: Display> {
    hyperlink: H,
    text: T,
}

impl<H: Display, T: Display> Hyperlink<H, T> {
    pub fn new(hyperlink: H, text: T) -> Self {
        Self { hyperlink, text }
    }
}

impl<T: Display> Hyperlink<String, T> {
    pub fn from_path_named<P: Display>(file_path: P, name: T) -> Self {
        Hyperlink::new(format!("file://{}{file_path}", &*HOSTNAME), name)
    }

    pub fn from_path(file_path: T) -> Self {
        Hyperlink::new(format!("file://{}{file_path}", &*HOSTNAME), file_path)
    }
}

impl<H: Display, T: Display> Display for Hyperlink<H, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
            self.hyperlink, self.text
        )
    }
}

pub struct HighlightFile<'a, Ansi: Display>(pub &'a Utf8Path, pub Ansi);

impl<'a, Ansi: Display> Display for HighlightFile<'a, Ansi> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Some(file) = self.0.file_name() else {
            return Display::fmt(&self.0, f);
        };

        let Some(parent) = self.0.parent() else {
            return Display::fmt(&self.0, f);
        };

        write!(f, "{parent}/\x1b[{}m{file}\x1b[0m", self.1)
    }
}
