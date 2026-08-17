use super::*;

/// A `scrollWheel` event with a delta, aimed at a point in a window.
#[derive(Debug, Clone, Copy)]
pub struct PidScroll {
    pub pid: i32,
    pub point: (f64, f64),
    pub window_local: (f64, f64),
    pub wid: u32,
    /// Vertical delta. Positive scrolls *up* — that is, content moves down —
    /// which is the sign convention `CGEventCreateScrollWheelEvent2` uses.
    pub delta_y: i32,
    /// Horizontal delta. Positive scrolls left.
    pub delta_x: i32,
    pub unit: ScrollUnit,
    pub modifiers: CGEventFlags,
}

/// What a scroll delta is counted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollUnit {
    /// Points of content. What a trackpad sends, and what a web view or an
    /// Electron list expects.
    #[default]
    Pixel,
    /// Wheel notches. What a physical mouse wheel sends; a receiver is free to
    /// turn one line into any number of points.
    Line,
}

impl ScrollUnit {
    pub(super) fn cg(self) -> CGScrollEventUnit {
        match self {
            ScrollUnit::Pixel => CGScrollEventUnit::Pixel,
            ScrollUnit::Line => CGScrollEventUnit::Line,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScrollUnit::Pixel => "pixel",
            ScrollUnit::Line => "line",
        }
    }
}

/// How a scroll event is built before it is stamped and posted.
///
/// This exists because the wheel tier does not work and the reason is not known
/// (DESIGN §11). Every variant here is a hypothesis about *why* a pid-routed
/// `scrollWheel` is delivered and scrolls nothing, expressed as the smallest
/// change to the recipe that would falsify it, so the next person can re-run the
/// experiment by setting one environment variable rather than by editing this
/// file. They are not options a caller chooses between: [`Plain`] is what ships,
/// and the rest are instruments.
///
/// [`Plain`]: ScrollRecipe::Plain
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollRecipe {
    /// `CGEventCreateScrollWheelEvent2`, stamped and posted. What ships, and
    /// what was measured not to scroll.
    #[default]
    Plain,
    /// The same event handed to `+[NSEvent eventWithCGEvent:]` and taken back
    /// with `-[NSEvent CGEvent]`.
    ///
    /// The hypothesis: §6 found that a click only works when AppKit builds the
    /// event, because AppKit rebuilds its `NSEvent` from the record's own header
    /// rather than from fields a caller patched in, and `NSEvent` publishes no
    /// scroll-wheel factory to build one with. If the round trip is enough to
    /// attach whatever a factory would have attached, this scrolls and `Plain`
    /// does not.
    NsEventRoundTrip,
    /// One event, plus the fields a real trackpad carries:
    /// `kCGScrollWheelEventIsContinuous`, the point deltas, and a scroll phase.
    ///
    /// The hypothesis: a receiver that only honours a phased gesture rejects a
    /// phaseless one outright.
    Phased,
    /// Three events — phase `Began`, phase `Changed` carrying the delta, phase
    /// `Ended` — which is the shape of one real trackpad gesture.
    ///
    /// The hypothesis above, taken seriously: a receiver may need the *gesture*
    /// and not merely the phase field, since a lone `Changed` with no `Began`
    /// before it is not a state any real device produces.
    PhasedGesture,
}

impl ScrollRecipe {
    /// The spelling accepted in `CUA_WHEEL_RECIPE`.
    pub fn parse(name: &str) -> Option<ScrollRecipe> {
        match name.trim().to_ascii_lowercase().as_str() {
            "plain" | "" => Some(ScrollRecipe::Plain),
            "nsevent" | "ns" | "roundtrip" => Some(ScrollRecipe::NsEventRoundTrip),
            "phased" => Some(ScrollRecipe::Phased),
            "gesture" | "phased-gesture" => Some(ScrollRecipe::PhasedGesture),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScrollRecipe::Plain => "plain",
            ScrollRecipe::NsEventRoundTrip => "nsevent",
            ScrollRecipe::Phased => "phased",
            ScrollRecipe::PhasedGesture => "gesture",
        }
    }

    /// The recipe named by `CUA_WHEEL_RECIPE`, defaulting to [`Plain`].
    ///
    /// An unrecognized name is [`Plain`] rather than an error: this switch
    /// exists for an experiment, and an experiment that silently ran the wrong
    /// arm would be worse than one that ran the shipped arm — which is what the
    /// probe prints, so a typo is visible in the output rather than in the
    /// verdict.
    ///
    /// [`Plain`]: ScrollRecipe::Plain
    pub fn from_env() -> ScrollRecipe {
        static RECIPE: OnceLock<ScrollRecipe> = OnceLock::new();
        *RECIPE.get_or_init(|| {
            std::env::var("CUA_WHEEL_RECIPE")
                .ok()
                .and_then(|v| ScrollRecipe::parse(&v))
                .unwrap_or_default()
        })
    }
}
