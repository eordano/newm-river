//! The model's side of blocks (`crate::block`): placing one on the active canvas, its windows
//! taking their slots, the slots whose time passed, and parking the windows already open.

use std::time::{Duration, Instant};

use newm_canvas::{Id, Rect as Tiles};
use niri_ipc::NewmBlockAnchor;

use super::{Desktop, OutputId};
use crate::block::{self, Block};
use crate::placeholder::{Placeholder, RestoreWindow};

impl Desktop {
    /// Places a block of `windows` (their tiles relative to its origin) on the active canvas at
    /// the origin [`block::block_origin`] picks for `anchor`: a placeholder on each window's
    /// tiles, which the next window with its app ID takes (of several, the first placed) until
    /// `lifetime` passes. The focus waits until no placeholder of the block waits; then the
    /// window of slot `focus` takes it. The block's id and origin.
    pub fn place_block(
        &mut self,
        windows: Vec<RestoreWindow>,
        anchor: NewmBlockAnchor,
        focus: Option<usize>,
        fly: bool,
        lifetime: Duration,
        now: Instant,
    ) -> Result<(u64, (i32, i32)), String> {
        self.sync_screens();
        let out = self
            .active_canvas_output()
            .ok_or_else(|| String::from("no canvas"))?;
        let canvas = &self
            .output(out)
            .ok_or_else(|| String::from("no canvas"))?
            .canvas;
        let slots: Vec<Tiles> = windows.iter().map(|w| w.rect).collect();
        let origin = block::block_origin(canvas, &slots, anchor)?;
        let ids: Vec<Id> = windows.iter().map(|_| self.placeholders.alloc()).collect();
        self.apply(Some(out), |c| {
            block::reserve_block(c, &ids, &slots, origin, focus, fly);
        });
        for (&id, w) in ids.iter().zip(windows) {
            self.placeholders.push(Placeholder {
                id,
                output: out,
                app_id: w.app_id,
                title: None,
                icon: None,
                deadline: now + lifetime,
                created: now,
            });
        }
        block::make_room(&mut self.blocks);
        let b = Block::new(&ids, focus);
        let id = b.id();
        self.blocks.push(b);
        Ok((id, origin))
    }

    /// Per slot of block `id`, the window that took it and is still open, and whether every
    /// slot has a window or was released; none when there is no block `id`.
    pub fn block_status(&self, id: u64) -> Option<(Vec<Option<Id>>, bool)> {
        let b = self.blocks.iter().find(|b| b.id() == id)?;
        let windows = b
            .windows()
            .into_iter()
            .map(|w| w.filter(|w| self.windows.contains_key(w)))
            .collect();
        Some((windows, b.is_done()))
    }

    /// Whether `placeholder` holds a block's slot.
    pub(super) fn is_block_slot(&self, placeholder: Id) -> bool {
        self.blocks.iter().any(|b| b.holds(placeholder))
    }

    /// Window `window` took `placeholder`, when the placeholder holds a block's slot.
    pub(super) fn block_slot_taken(&mut self, placeholder: Id, window: Id) {
        for b in &mut self.blocks {
            b.take(placeholder, window);
        }
    }

    /// The placeholders `expired` are gone: their block slots are released.
    pub(super) fn block_slots_expired(&mut self, expired: &[Id]) {
        for b in &mut self.blocks {
            b.release(expired);
        }
    }

    /// Each newly complete block gives the focus to the window it names, which shows it.
    /// Whether a block completed.
    pub(super) fn settle_blocks(&mut self, now: Instant) -> bool {
        let settled: Vec<Option<Id>> = self.blocks.iter_mut().filter_map(Block::settle).collect();
        for &id in settled.iter().flatten() {
            if self.windows.contains_key(&id) {
                self.focus_window(id, now);
            }
        }
        !settled.is_empty()
    }

    /// Parks the active canvas's grid windows (see [`block::park`]): the focus goes to the
    /// parked window nearest the view. One undo takes the whole park back. False when the
    /// canvas holds no window.
    pub fn park_windows(&mut self, now: Instant) -> bool {
        let out = self.active_canvas_output();
        let Some(Some(park)) = self.logged("park", out, None, block::park) else {
            return false;
        };
        self.park = out.map(|o| (o, park));
        self.follow_canvas(out, now);
        true
    }

    /// Places the grid window `id` on `tile` of its canvas with `span` ([`block::place_window`]):
    /// windows on those tiles make room, as for a drop there. The tiles it holds then; one undo
    /// takes the whole placement back. An error, with nothing changed, when there is no window
    /// `id` or it is not on the grid (floating, fullscreen, or not placed yet).
    pub fn place_window(
        &mut self,
        id: Id,
        tile: (i32, i32),
        span: Option<(i32, i32)>,
        now: Instant,
    ) -> Result<Tiles, String> {
        let w = self.windows.get(&id).ok_or("no such window")?;
        let out = w.output;
        let on_grid = w.is_tiled()
            && !w.fullscreen
            && out
                .and_then(|o| self.output(o))
                .is_some_and(|o| o.canvas.rect(id).is_some());
        if !on_grid {
            return Err("the window is not on the grid".into());
        }
        let placed = self
            .logged("place", out, Some(id), |c| {
                block::place_window(c, id, tile, span)
            })
            .flatten()
            .ok_or("the window is not on the grid")?;
        if placed.changed() {
            self.park = out.map(|o| (o, placed));
        }
        self.follow_canvas(out, now);
        out.and_then(|o| self.output(o))
            .and_then(|o| o.canvas.rect(id))
            .ok_or_else(|| "the window is not on the grid".into())
    }

    /// How many history steps one undo (or, with `redo`, one redo) on `out`'s canvas takes:
    /// the last park's while they are the last steps there ([`block::Park::steps`]).
    pub(super) fn park_steps(&self, out: Option<OutputId>, redo: bool) -> Option<usize> {
        let (o, park) = self.park.as_ref()?;
        let canvas = &self.output(out.filter(|out| out == o)?)?.canvas;
        park.steps(canvas, redo)
    }
}

#[cfg(test)]
mod tests {
    use newm_canvas::Config as CanvasConfig;
    use niri_config::Action;

    use super::super::{Rect, Settings};
    use super::*;

    fn desktop() -> (Desktop, Instant) {
        let now = Instant::now();
        let settings = Settings {
            canvas: CanvasConfig {
                tile_w: 119.,
                tile_h: 113.,
                ..CanvasConfig::default()
            },
            ..Settings::default()
        };
        let mut d = Desktop::new(settings, Vec::new(), now);
        d.add_output(1);
        d.set_output_area(1, Rect::new(0, 0, 1920, 1080));
        (d, now)
    }

    /// A window of `app_id` opens as river would: a manage sequence, the size it was asked for
    /// committed.
    fn open(d: &mut Desktop, id: Id, app_id: &str, now: Instant) -> Tiles {
        d.add_window(id);
        d.window_mut(id).unwrap().app_id = Some(app_id.into());
        d.manage(now);
        for (id, size) in d.proposals() {
            d.set_asked(id, size);
            d.on_dimensions(id, size.0, size.1);
        }
        tiles(d, id)
    }

    fn tiles(d: &Desktop, id: Id) -> Tiles {
        d.output(1).unwrap().canvas.rect(id).unwrap()
    }

    fn slot(app_id: &str, rect: Tiles) -> RestoreWindow {
        RestoreWindow {
            app_id: app_id.into(),
            title: None,
            rect,
        }
    }

    /// A terminal two tiles wide and a browser two tiles tall right of it.
    fn two_slots() -> Vec<RestoreWindow> {
        vec![
            slot("term", Tiles::new(0, 0, 2, 1)),
            slot("browser", Tiles::new(2, 0, 1, 2)),
        ]
    }

    fn place(
        d: &mut Desktop,
        anchor: NewmBlockAnchor,
        focus: Option<usize>,
        now: Instant,
    ) -> Result<(u64, (i32, i32)), String> {
        d.place_block(
            two_slots(),
            anchor,
            focus,
            true,
            Duration::from_secs(20),
            now,
        )
    }

    /// A block's slots match an app whatever the case of its id: a block saved
    /// where an app runs under X11 fills where it runs on Wayland.
    #[test]
    fn block_slots_match_an_app_whatever_the_case_of_its_id() {
        let (mut d, now) = desktop();
        let slots = vec![
            slot("Chromium-browser", Tiles::new(0, 0, 2, 1)),
            slot("term", Tiles::new(2, 0, 1, 2)),
        ];
        let (id, origin) = d
            .place_block(
                slots,
                NewmBlockAnchor::Tiles(20, 0),
                None,
                true,
                Duration::from_secs(20),
                now,
            )
            .unwrap();
        assert_eq!(origin, (20, 0));
        assert_eq!(open(&mut d, 1, "Term", now), Tiles::new(22, 0, 1, 2));
        assert_eq!(
            open(&mut d, 2, "chromium-browser", now),
            Tiles::new(20, 0, 2, 1)
        );
        assert_eq!(d.block_status(id), Some((vec![Some(2), Some(1)], true)));
        assert!(d.placeholder_frames().is_empty());
    }

    #[test]
    fn a_block_at_tiles_lands_there_and_its_windows_take_their_slots() {
        let (mut d, now) = desktop();
        open(&mut d, 1, "other", now);
        let (id, origin) = place(&mut d, NewmBlockAnchor::Tiles(20, 0), Some(1), now).unwrap();
        assert_eq!(origin, (20, 0));
        assert_eq!(d.block_status(id), Some((vec![None, None], false)));
        assert_eq!(d.placeholder_frames().len(), 2);

        assert_eq!(open(&mut d, 2, "browser", now), Tiles::new(22, 0, 1, 2));
        assert_eq!(open(&mut d, 3, "term", now), Tiles::new(20, 0, 2, 1));
        assert_eq!(
            d.focus(),
            Some(2),
            "a window of the block takes the focus only at its end"
        );
        assert_eq!(d.block_status(id), Some((vec![Some(3), Some(2)], true)));
        assert!(d.placeholder_frames().is_empty());
        assert_eq!(d.block_status(id + 1000), None);

        assert_eq!(
            place(&mut d, NewmBlockAnchor::Tiles(21, 0), None, now),
            Err(String::from("the block's tiles are taken"))
        );
        assert_eq!(
            d.place_block(
                Vec::new(),
                NewmBlockAnchor::Nearest,
                None,
                true,
                Duration::ZERO,
                now
            ),
            Err(String::from("bad block"))
        );
    }

    #[test]
    fn a_nearest_block_keeps_a_tile_clear_and_a_camera_block_centres() {
        let (mut d, now) = desktop();
        let held = open(&mut d, 1, "other", now);
        let (_, (i, j)) = place(&mut d, NewmBlockAnchor::Nearest, None, now).unwrap();
        let wide = Tiles::new(i - 1, j - 1, 5, 4);
        assert!(!held.overlaps(&wide), "{held:?} and the block at {i},{j}");

        let (mut d, now) = desktop();
        let (_, origin) = place(&mut d, NewmBlockAnchor::Camera, None, now).unwrap();
        let c = &d.output(1).unwrap().canvas;
        let (tw, th) = c.tile_size();
        let (sw, sh) = c.screen();
        let want = (
            (sw / tw / 2. - 1.5).round_ties_even() as i32,
            (sh / th / 2. - 1.).round_ties_even() as i32,
        );
        assert_eq!(origin, want);
    }

    #[test]
    fn a_slot_no_window_takes_is_released() {
        let (mut d, now) = desktop();
        let (id, _) = place(&mut d, NewmBlockAnchor::Tiles(0, 0), Some(1), now).unwrap();
        open(&mut d, 1, "term", now);
        assert!(!d.tick(now + Duration::from_secs(19)));
        assert_eq!(d.block_status(id), Some((vec![Some(1), None], false)));
        assert!(d.tick(now + Duration::from_secs(20)));
        assert_eq!(d.block_status(id), Some((vec![Some(1), None], true)));
        assert_eq!(d.output(1).unwrap().canvas.windows().len(), 1);
    }

    #[test]
    fn the_park_keeps_the_arrangement_left_of_the_view_and_undoes_in_one_step() {
        let (mut d, now) = desktop();
        d.restore(
            None,
            crate::placeholder::RestoreCanvas {
                windows: vec![
                    slot("a", Tiles::new(0, 0, 1, 1)),
                    slot("b", Tiles::new(1, 0, 1, 1)),
                    slot("c", Tiles::new(2, 0, 1, 1)),
                    slot("d", Tiles::new(1, 1, 2, 1)),
                ],
                zoom: 0,
                view: (0., 0.),
                focus: Some(0),
            },
            Duration::from_secs(20),
            now,
        );
        for (id, app) in [(1, "a"), (2, "b"), (3, "c"), (4, "d")] {
            open(&mut d, id, app, now);
        }
        let before: Vec<Tiles> = (1..=4).map(|id| tiles(&d, id)).collect();
        let view = d.output(1).unwrap().canvas.view();
        assert!(d.park_windows(now));
        let after: Vec<Tiles> = (1..=4).map(|id| tiles(&d, id)).collect();
        let di = after[0].i - before[0].i;
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(
                *a,
                Tiles::new(b.i + di, b.j, b.w, b.h),
                "the arrangement stays"
            );
        }
        let c = &d.output(1).unwrap().canvas;
        let left = (view.0 / c.tile_size().0).floor() as i32;
        assert_eq!(after.iter().map(|r| r.right()).max(), Some(left - 1));
        let focus = d.focus().unwrap();
        assert_eq!(
            tiles(&d, focus).right(),
            left - 1,
            "the focus goes to a window nearest the view"
        );
        assert!(c.is_visible(tiles(&d, focus)), "which the camera shows");

        d.act(Action::Undo, now);
        assert_eq!((1..=4).map(|id| tiles(&d, id)).collect::<Vec<_>>(), before);
        d.act(Action::Redo, now);
        assert_eq!((1..=4).map(|id| tiles(&d, id)).collect::<Vec<_>>(), after);
    }

    fn row_of_three() -> (Desktop, Instant) {
        let (mut d, now) = desktop();
        d.restore(
            None,
            crate::placeholder::RestoreCanvas {
                windows: vec![
                    slot("a", Tiles::new(0, 0, 1, 1)),
                    slot("b", Tiles::new(1, 0, 1, 1)),
                    slot("c", Tiles::new(2, 0, 1, 1)),
                ],
                zoom: 0,
                view: (0., 0.),
                focus: Some(0),
            },
            Duration::from_secs(20),
            now,
        );
        for (id, app) in [(1, "a"), (2, "b"), (3, "c")] {
            open(&mut d, id, app, now);
        }
        (d, now)
    }

    fn all(d: &Desktop) -> Vec<Tiles> {
        (1..=3).map(|id| tiles(d, id)).collect()
    }

    #[test]
    fn a_placed_window_lands_on_taken_tiles_as_a_drop_there_does() {
        let (mut d, now) = row_of_three();
        let before = all(&d);
        let focus = d.focus();
        let mut dropped = d.output(1).unwrap().canvas.clone();
        dropped.move_to(3, 0, 0);

        let placed = d.place_window(3, (0, 0), None, now);
        assert_eq!(
            placed,
            Ok(Tiles::new(0, 0, 1, 1)),
            "it takes the tiles asked"
        );
        assert_ne!(
            tiles(&d, 1),
            before[0],
            "the window that held them makes room"
        );
        let c = &d.output(1).unwrap().canvas;
        c.check().unwrap();
        for id in 1..=3 {
            assert_eq!(
                c.rect(id),
                dropped.rect(id),
                "as one Canvas::move_to arranges them"
            );
        }
        assert_eq!(d.focus(), focus, "the focus stays");

        d.act(Action::Undo, now);
        assert_eq!(all(&d), before);
        assert_eq!(d.place_window(3, (2, 0), None, now), Ok(before[2]));
        assert_eq!(all(&d), before, "its own tiles change nothing");
        assert_eq!(
            d.place_window(99, (0, 0), None, now),
            Err("no such window".into())
        );
    }

    #[test]
    fn a_placed_window_takes_its_span_and_undoes_in_one_step() {
        let (mut d, now) = row_of_three();
        let before = all(&d);
        let placed = d.place_window(2, (5, 3), Some((2, 3)), now);
        assert_eq!(placed, Ok(Tiles::new(5, 3, 2, 3)));
        let after = all(&d);
        d.act(Action::Undo, now);
        assert_eq!(all(&d), before, "one undo takes the move and the span back");
        d.act(Action::Redo, now);
        assert_eq!(all(&d), after, "one redo places again");
        assert_eq!(
            d.place_window(2, (5, 3), Some((1, 1)), now),
            Ok(Tiles::new(5, 3, 1, 1)),
            "a span alone resizes in place"
        );

        let big = d.place_window(1, (0, 8), Some((500, 500)), now).unwrap();
        let c = &d.output(1).unwrap().canvas;
        c.check().unwrap();
        assert_eq!((big.i, big.j), (0, 8));
        assert!(
            c.fits_at_1to1(big.w, big.h),
            "no larger than the screen at 1:1"
        );
        assert!(!c.fits_at_1to1(big.w + 1, big.h) && !c.fits_at_1to1(big.w, big.h + 1));
    }

    #[test]
    fn the_window_list_carries_tiles_and_a_floating_window_is_not_placed() {
        let (mut d, now) = row_of_three();
        for id in 1..=3 {
            let r = tiles(&d, id);
            let w = d.ipc_window(d.window(id).unwrap());
            assert!(!w.is_floating);
            assert_eq!(w.layout.tiles, Some((r.i, r.j, r.w, r.h)));
            assert_eq!(w.workspace_id, Some(1), "the canvas is the output's");
        }
        d.place_window(2, (7, 5), None, now).unwrap();
        let w = d.ipc_window(d.window(2).unwrap());
        assert_eq!(
            w.layout.tiles,
            Some((7, 5, 1, 1)),
            "the list follows a move"
        );

        let focus = d.focus().unwrap();
        d.act(Action::ToggleWindowFloating, now);
        let others: Vec<Id> = (1..=3).filter(|id| *id != focus).collect();
        let before: Vec<Tiles> = others.iter().map(|&id| tiles(&d, id)).collect();
        let w = d.ipc_window(d.window(focus).unwrap());
        assert!(w.is_floating, "a floating window says so");
        assert_eq!(w.layout.tiles, None, "and holds no tiles");
        assert_eq!(
            d.place_window(focus, (0, 0), None, now),
            Err("the window is not on the grid".into())
        );
        let after: Vec<Tiles> = others.iter().map(|&id| tiles(&d, id)).collect();
        assert_eq!(after, before, "a refused request moves nothing");
    }
}
