# Draft issue for zed-industries/zed

Everything below the line is the issue body — copy it as-is. The suggested title is:

> **gpui: an observer cycle live-locks `flush_effects` with no error**

Labels that seem right: `gpui`, `bug`. This is a new report — no open issue describes
this mechanism, and no PR proposes cycle detection in `flush_effects`.

---

### Summary

`App::flush_effects` drains `pending_effects` in an unbounded loop. If the observer
graph has a cycle in it — A observes B, B observes C, something in C's handler
notifies A — the queue refills as fast as it drains and the loop never ends.

The failure mode is the problem: there is no panic, no error, and nothing in the log
except the hang detector reporting that a task ran long. One core sits at 100%, no
further frames are drawn, and on macOS the window server marks the app Not
Responding. If the cycle forms while panels are being added at startup, this happens
*before the first window appears*, so the app looks like it failed to launch.

Nothing in gpui says a cycle is what went wrong, and nothing says which entities are
involved.

### Why this is worth catching in gpui

Cycles are easy to create by accident and hard to see in review, because each edge
is locally reasonable. The one I hit was four hops, three of which are Zed's own:

```
  MyPanel --cx.observe(multi_workspace)--> notify
     ^                                       |
     |                                       v
MultiWorkspace <--observes Sidebar-- Sidebar <--observes every Dock-- Dock
     ^                                                                 |
     +-------------- dock.rs observes each of its panels --------------+
```

- `Dock` observes each of its panels and calls `cx.notify()` (`crates/workspace/src/dock.rs`)
- `Sidebar` observes every dock and calls `cx.notify()` (`crates/sidebar/src/sidebar.rs`)
- `MultiWorkspace` observes the sidebar and calls `cx.notify()` (`crates/workspace/src/multi_workspace.rs`)

Each of those is fine on its own. A panel that observes `MultiWorkspace` — which is a
natural thing for a panel that renders the workspace list to do — closes the ring, and
the window then never finishes its first flush. The fix on my side was one line
(subscribe to an event instead of observing), but finding it took a process sample and
a couple of hours, because the app gave no indication of what it was doing.

A panel author has no way to know that `cx.notify()` on a panel is not a leaf.

### Reproduction

This test hangs forever on `main`:

```rust
#[gpui::test]
fn a_notify_cycle_ends_instead_of_spinning(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let first = cx.new(|_| Counter);
        let second = cx.new(|_| Counter);

        cx.observe(&first, {
            let second = second.clone();
            move |_, cx| second.update(cx, |_, cx| cx.notify())
        })
        .detach();
        cx.observe(&second, {
            let first = first.clone();
            move |_, cx| first.update(cx, |_, cx| cx.notify())
        })
        .detach();

        first.update(cx, |_, cx| cx.notify());
    });
}

struct Counter;
```

I let it run for 200 seconds before killing it.

### Proposed fix

Count notifications applied per entity within a single `flush_effects` call. Past a
threshold, log an error naming the entity and drop its further notifications for the
rest of that flush.

Dropping is safe, and this is the part that makes the fix cheap: notifications are
**already coalesced** — `push_effect` keeps `pending_notifications` as a set, so an
entity can only be pending once. Reaching the threshold therefore means the entity was
notified, its observers ran, and something they did notified it again, N times over, all
within one update. The N+1th notification adds nothing its observers have not already
done N times on the same update.

One detail that matters: the skip path must still remove the entity from
`pending_notifications`. Leaving it set would make `push_effect` silently discard every
future notification for that entity for the rest of the process — a worse bug than the
one being fixed.

Every entity in a cycle trips the counter, so the error lines name the whole ring rather
than one arbitrary member.

Sketch (against `crates/gpui/src/app.rs`):

```rust
/// How many times one entity may be notified while a single update's effects
/// are flushed before that is treated as a cycle rather than as work.
const MAX_NOTIFIES_PER_FLUSH: usize = 1024;

fn flush_effects(&mut self) {
    let mut notifies: FxHashMap<EntityId, usize> = FxHashMap::default();
    loop {
        // ...
        Effect::Notify { emitter } => {
            let notified = notifies.entry(emitter).or_default();
            *notified += 1;
            match (*notified).cmp(&MAX_NOTIFIES_PER_FLUSH) {
                Ordering::Less => self.apply_notify_effect(emitter),
                Ordering::Equal => {
                    log::error!(
                        "notify cycle: entity {emitter:?} has been notified \
                         {MAX_NOTIFIES_PER_FLUSH} times while flushing one update, \
                         so some chain of `cx.observe(..)` and `cx.notify()` leads \
                         back to it. ..."
                    );
                    // Leaving this set would make `push_effect` drop this
                    // entity's notifications for the rest of the process.
                    self.pending_notifications.remove(&emitter);
                }
                Ordering::Greater => {
                    self.pending_notifications.remove(&emitter);
                }
            }
        }
        // ...
    }
}
```

The cost below the threshold is one hash map entry and one increment per notification.

### What I have measured

- The test above runs for 200s+ unbounded, and passes in 0.02s with the guard.
- The full `gpui` suite passes with the change (340 tests), as does `workspace` (277).
- No false positives so far, though this is light evidence: ~25 launches of a build
  carrying the guard, each a few minutes — startup, opening and closing projects,
  switching worktrees, resizing docks — with zero `notify cycle` lines logged. I have not
  run it for days, and I have not exercised the whole editor.

### Open questions for maintainers

1. **Threshold.** 1024 is a guess that is clearly above legitimate use and clearly below
   "forever". Would you want it lower, or configurable?
2. **Drop vs. panic.** I chose to log and continue, so a release build degrades into a
   usable app rather than dying. `debug_assert!` on top of that would make cycles loud
   in development — happy to add it if that is the preference.
3. **Naming the entity.** The error prints an `EntityId`, which is not very actionable.
   A type name would be, but `EntityMap` only keeps type names under `test` /
   `leak-detection`. Is it worth keeping a `TypeId → &'static str` map in release builds
   for diagnostics like this one?

### Not the cause of the open CPU reports, as far as I can tell

I checked #64575 ("200% CPU infinite loop") against this hypothesis and it does **not**
look like an effect cycle: of its 79 hang reports, 78 are *background* hangs (58 of them
at `app/context.rs:861`, i.e. `background_spawn`) and only one is a foreground hang,
which lasted 771ms and ended. The app kept logging normally for 16 minutes. An effect
cycle pins the main thread, so it would show repeated *foreground* hangs and no further
frames — and it pins exactly one core, not two.

I mention it because the diagnostic gap is the same: that reporter says "not really sure
what causes it", and nothing in the log would distinguish an effect cycle from the
background-work storm they actually seem to have. This change would rule the cycle in or
out from the log alone.

#57585 (100% CPU, tree-sitter reparse loop), #50283, #64380 and #34302 are the same
"pinned CPU, no diagnosis" shape with, again, different root causes.

I am happy to open a PR with the change and the test.
