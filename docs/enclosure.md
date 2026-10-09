# Enclosures

[`cad/models.py`](../cad/models.py) is the CadQuery source for every printed
part; the exports are in [`cad/export/`](../cad/export/).

| Parts | For |
| --- | --- |
| `terrasse_body`, `_floor`, `_charger_tray`, `_anchor` | the bird-scale node, outdoors |
| `terrasse_beam_spacer`, `_beam_hanger` | the clamps the scale's bending beam hangs from |
| `wohnzimmer_tray`, `_lid` | the air-quality node |
| `schlafzimmer_tray`, `_lid` | the CO₂ node |
| `climate_tray`, `_lid` | `kueche` and `bad`, the plain climate nodes |
| `solarleuchte_body`, `_lid` | the garden lamp's electronics (#48) |
| `wasserzaehler_tube`, `_strap`, `_cap`, `_bar` | a camera tube for the water meters — not a node, see below |

Only the two terrasse groups are described below; the rest are documented in the
comments above their section of `cad/models.py`.

The clamps the file *started* as were retired in `f858d60` and are not
these — `git show d0df819:models.py` still has the originals, which put the wire
hole where the bar puts it rather than on the box's centre line.

The `wasserzaehler_*` parts are the one real exception to the naming rule. Every
other part here is named after a `NODE=` slug (`climate` after the pair of them) from [`src/node.rs`](../src/node.rs),
and `wasserzaehler` is not one and never will be: it is a camera tube for the
flat's two water meters, which are read by an ESP32-CAM running jomjol's
AI-on-the-edge-device, not by this crate. It lives here because this is where
the CAD toolchain lives, and a second copy of it elsewhere would be worse than
one stretched convention. The meters, the board criteria and the reasoning
behind the camera route are in `nixos-private/docs/zaehler.md`.

```bash
nix develop .#cad        # separate shell: OpenCASCADE is ~500 MB, cargo has no use for it
python cad/models.py     # writes cad/export/*.stl and *.step
```

CadQuery is not in nixpkgs (only the `opencascade-occt` kernel, without the
Python bindings), so the shell pins the wheels and installs them into
`.venv-cad` on first entry (ignored). The exports under `cad/export/` are
tracked, so printing a part does not require the toolchain at all — `.stl` to
slice, `.step` to open in CAD.

## `terrasse_body` + `terrasse_floor` + `terrasse_charger_tray` + `terrasse_anchor`

An **88 × 78 × 71 mm** box for the bird scale, in four parts. The size is not a
choice — it comes from the measured envelopes, plugs included:

| | | | |
|---|---|---|---|
| ESP32-C3 board | 40 × 30 × 35 | HX711 board | 40 × 25 × 30 |
| battery | 65 × 50 × 10 | SHT31 | 20 × 10 × 10 |

That is 128 cm³ with clearance. The first version of this box had a 98 cm³
interior, so no amount of rearranging would have done it; a grid packing search
puts the smallest interior that takes all four at 76 × 66 × 52, and only with
everything stood on end. This one is 80 × 70 × 66, which leaves room for the
mounts themselves — and, since 2026-09-18, for a fifth part the floor could
never have held: the solar charger, on a shelf above the other two boards.

The design follows from it hanging outdoors in the rain:

- **The roof has no seam and no penetration.** The box is a cup opening
  *downward*; the only joint faces the ground, where water cannot climb to it.
  The top 5 mm flare 2 mm past the walls as an eave, so water crossing the roof
  drips clear instead of running down the sides. Nothing is drilled through the
  top — the cord attaches to two round-ended tabs *outboard* of the walls,
  which is why the part reaches 108 mm across.
- **The SHT31 lives in the −X/−Y corner, no longer behind a wall.** Air enters
  through floor slots beneath it and leaves through slots high in *both*
  adjacent walls, tilted 30° down-and-out — every opening faces down or
  outward-down, so there is still a local draught across the sensor.

  There *was* a baffle around it, and it came out because the cable could not
  be got past it during assembly: the slot was widened, then run full height,
  and it still fouled. That costs accuracy, since the board's heat now shares
  the sensor's air — about 0.9 °C on the bedroom node. A box that cannot be
  assembled is worse than one that reads half a degree warm, but keep the SHT31
  in that corner rather than beside the board, or the loss compounds.
- **The floor screws go into M3 heat-set inserts**, Ø4 bores in 9 mm posts,
  which leaves 2.5 mm of wall — thinner than one would choose, and what the
  already-printed floor plate allows.

  The screw positions are not free: they are where the 8 mm post this box
  shipped with put them, and the plate in hand has its countersinks there. So
  the post has to grow *around* a fixed hole, and it meets two features of that
  same plate — the cell reaches x = 31 and the battery guides reach y = ±26.
  Nine millimetres leaves 0.5 mm to both; ten does not fit. The plate's HX711
  rib still fouled one post by 0.5 mm, so that post is relieved there rather
  than shrunk: the rib only has to slide past, the wall has to survive brass
  being melted into it. Warm the inserts properly and do not lean on them; if
  one splits, the way out is a reprinted floor with the holes further in, which
  buys 3.5 mm.
- **The load hangs off a pad** matching the clamp's 25 × 12 face and its screw
  pitch. Two **M4 × 25 countersunk** screws run *down* from inside the box —
  heads countersunk into the nut boss, nuts in a hex pocket under the beam
  spacer, which is the end you can still reach with a spanner once the box is
  together. They ran the other way until 2026-09-18, with captive nuts in
  pockets inside the box; a nut that drops out during assembly is a nut inside
  a sealed enclosure. That pad is
  `terrasse_anchor`, its own part since 2026-09-18 and clamped under the plate
  by the same two M4 screws. It was part of the plate until then, and it was
  the only thing on that face: while it was there the plate could not be
  printed without support, because an 84 × 74 plate laid anchor-down steps out
  over a 33 × 18 flange. Now **no part of this box needs support** — body roof
  down, floor plate down, anchor pad down.
- **The solar charger sits on a tray over the two boards**, on three columns
  rising from the floor plate, and the panel lead comes in through the floor.
  Both are why the box is 71 mm tall rather than 60. See
  [`solar.md`](solar.md).

## `terrasse_beam_spacer` + `terrasse_beam_hanger`

The two clamps that pad exists for. The **spacer** bolts up into the floor's
captive nuts and carries the bar's fixed end; the **hanger** grips the load end,
and the wire leaves from its far end.

Two parts rather than one, because both ends of the bar share the anchor's 15 mm
screw pitch — a single pair of screws cannot fasten the box and the bar at once.
The spacer offsets the fixed end until the pairs clear. `BEAM_DIR` decides which
way the bar runs from there; flipping its sign mirrors the whole assembly, and
since every feature sits on `y = 0`, an already-printed part just turns round.

The hanger is 80 mm, the bar's own length, with the M5 bolts at one end and the
wire hole at the other, **61.5 mm apart**. The wire leaving from the far end is
a requirement — it is where the feeder hangs — and the section is dimensioned
for it rather than around it.

What follows is a cantilever in the load path, which is why the **spine** is the
deep part at 20 mm and not the pad: stiffness goes with depth cubed. Deflection
here is an accuracy problem rather than a strength one — what bends does not
spring back exactly, and that shows up as hysteresis in the weight. If readings
ever differ between a loaded and an unloaded pan, deepen `RAIL_T` before
suspecting anything in the firmware.

**`RAIL_T` was 14 mm until 2026-09-27, and that was not enough.** Over three warm
days the terrace zero walked from +1.4 g to −75.8 g at matched air temperature and
did not return overnight — creep in the printed spine, not deflection. See
[`annotations.md`](annotations.md). 20 mm is 2.9× the stiffness and, since
bending stress goes with the square of the depth, about half the stress. It grows
*downward* into open air, so the 4 mm clearance below is untouched.

**The material matters more than the number.** That spine was PLA, which yields
under sustained load far below its glass transition, and a dark part in direct sun
runs well above the shaded air the SHT31 reads. **Print the hanger and the spacer
in PETG at the least, ASA for preference.** No `RAIL_T` survives PLA in August sun.

> **Nothing but the bar may bridge the two clamps.** Only the hanger's pad
> touches the bar; the spine runs 4 mm clear of it along its whole length and
> 16.7 mm clear of the spacer. If anything touches, the load path goes around
> the strain gauges and the scale reads a fraction of the weight — or none of
> it — without any sign that something is wrong.

**Printing.** The pad reaches down to the rail's underside rather than standing
proud of it, so there is one flat face across all 80 mm and a single 4 mm step
on top — a step *up*, overhanging nothing. The only downward faces off the bed
are the two counterbore ceilings, 112.9 mm² of bridge over a 5.3 mm hole. No
support. The pad is 24 mm thick as a result, and the bolts are **still M5 × 16** —
the counterbore swallows whatever the pad gains, so it deepens with `RAIL_T`
instead of asking for a longer bolt. What the M5 spans is the 8 mm of
`BOLT_BEARING` plus the bar's thread, which is why M5 × 12 does not reach. The
counterbore is now 16 mm deep at 10 mm across, so bring a hex key with the reach.

Calibrate in the fixture: `scale_factor` is what absorbs whatever the mounting
does to the sensitivity.
