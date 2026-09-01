pub enum Parent<'left> {
    Inherit,
    Explicit(&'left tracing::Span),
    Root,
}

#[must_use = "dropping a Region immediately closes its span as a zero-length frame"]
pub struct Region(tracing::Span);

impl Region {
    pub fn from_span(span: tracing::Span) -> Self {
        Self(span)
    }

    pub fn close(self) {}

    pub fn span(&self) -> &tracing::Span {
        &self.0
    }
}

#[macro_export]
macro_rules! region {
    ($name:literal, $parent:expr) => {
        $crate::region::Region::from_span(match $parent {
            $crate::region::Parent::Inherit => tracing::info_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::info_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::info_span!(parent: None, $name),
        })
    };
    (debug, $name:literal, $parent:expr) => {
        $crate::region::Region::from_span(match $parent {
            $crate::region::Parent::Inherit => tracing::debug_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::debug_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::debug_span!(parent: None, $name),
        })
    };
}

#[macro_export]
macro_rules! instrument_task {
    ($name:literal, $parent:expr, $fut:expr) => {
        tracing::Instrument::instrument($fut, match $parent {
            $crate::region::Parent::Inherit => tracing::info_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::info_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::info_span!(parent: None, $name),
        })
    };
    (debug, $name:literal, $parent:expr, $fut:expr) => {
        tracing::Instrument::instrument($fut, match $parent {
            $crate::region::Parent::Inherit => tracing::debug_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::debug_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::debug_span!(parent: None, $name),
        })
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span_profile::test_support::folded_with_layer;

    #[test]
    fn regions_fold_under_their_declared_parents() {
        let folded = folded_with_layer(|| {
            let outer = region!("outer", Parent::Inherit);
            let inner = region!("inner", Parent::Explicit(outer.span()));
            std::thread::sleep(std::time::Duration::from_millis(2));
            inner.close();
            let lone = region!("lone", Parent::Root);
            std::thread::sleep(std::time::Duration::from_millis(2));
            lone.close();
            let first = region!("first_wait", Parent::Explicit(outer.span()));
            std::thread::sleep(std::time::Duration::from_millis(2));
            first.close();
            outer.close();
        });
        let paths: Vec<&str> = folded
            .lines()
            .filter_map(|line| line.rsplit_once(' ').map(|(path, _)| path))
            .collect();
        assert!(paths.contains(&"outer"), "{folded}");
        assert!(paths.contains(&"outer;inner"), "{folded}");
        assert!(paths.contains(&"outer;first_wait"), "{folded}");
        assert!(paths.contains(&"lone"), "{folded}");
        for path in &paths {
            assert!(
                !matches!(*path, "inner" | "first_wait"),
                "child folded as root: {path}"
            );
            let segs: Vec<&str> = path.split(';').collect();
            assert!(
                segs.windows(2).all(|window| window[0] != window[1]),
                "duplicate frame: {path}"
            );
        }
    }
}
