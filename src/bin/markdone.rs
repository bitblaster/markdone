//! The Markdone editor binary.
//! Thin wrapper; all logic lives in `markdone::shell`.

fn main() -> anyhow::Result<()> {
    markdone::shell::run()
}
