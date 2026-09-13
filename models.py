import cadquery as cq
from cadquery import exporters
from pathlib import Path
import math
import re


path_save = Path("cad-models")
path_save.mkdir(exist_ok=True)

try:  # provided by CQ-editor / jupyter-cadquery; a no-op when run as a script
    display
except NameError:

    def display(*_args, **_kwargs):
        pass


## ===========================================================================
## Terrasse — outdoor housing for the bird-scale node
## ===========================================================================
##
## Two printed parts:
##
##   terrasse_body   walls + closed top. Open at the BOTTOM.
##   terrasse_floor  bottom plate. Carries the anchor and every mount.
##
## Sized from the measured envelopes, plugs included:
##
##   ESP32-C3 board   40 x 30 x 35      battery   65 x 50 x 10
##   HX711 board      40 x 25 x 30      SHT31     20 x 10 x 10
##
## That is 128 cm3 with clearance. The first version of this box had a 98 cm3
## interior, so it was not a matter of rearranging -- a grid packing search
## puts the smallest interior that takes all four at 76 x 66 x 52, and only
## with everything stood on end. This one is 80 x 70 x 55, which leaves room
## for the mounts themselves.
##
## The four requirements, and how each is met:
##
##   a) waterproof from above  There is no seam and no penetration in the roof.
##      The top is one integral wall, and the only joint faces down, where
##      water cannot climb to it. The top 5 mm flare 2 mm past the walls as an
##      eave, so water crossing the roof leaves the box clear of the side
##      walls instead of running down them.
##   b) cord attachment        Two round-ended tabs at roof level, outboard of
##      the walls. One cord through both makes a bail: it hangs level and
##      pierces nothing. Load path is tab -> wall -> floor -> anchor, in line.
##   c) beam anchor            A pad on the underside of the floor, matching
##      the bending-beam clamp (25 x 12 face, two screws 15 mm apart), with
##      captive nuts reachable from inside the box.
##   d) ventilation            An L-shaped chamber in the -X/-Y corner, walled
##      off from the electronics so the board's own heat does not reach the
##      SHT31. Air enters through slots in the floor and leaves through slots
##      high in both adjacent walls, tilted 30 deg down-and-out: a chimney
##      whose every opening faces down or outward-down.
##
## Everything mounts to the floor plate and stands up from it. That is not
## tidiness -- the body prints roof-down, so any horizontal feature inside it
## would be printing over thin air. The floor plate prints anchor-down, where
## ribs, rails and columns are all free.
##
## Print the body with the ROOF ON THE BUILD PLATE (opening up): every feature
## is then vertical or steps inward, so nothing needs support and the roof
## gets the smooth plate-side surface.

# --- envelope -------------------------------------------------------------
ENV_X, ENV_Y, ENV_Z = 88.0, 78.0, 60.0   # length, width, height

WALL = 2.0        # side and roof wall
FLOOR_T = 3.0     # floor plate
EAVE = 2.0        # how far the drip edge stands proud of the wall
EAVE_H = 5.0      # height of the drip-edge band

BODY_X = ENV_X - 2 * EAVE        # 84, wall outside
BODY_Y = ENV_Y - 2 * EAVE        # 74
IN_X = BODY_X - 2 * WALL         # 80, usable inside
IN_Y = BODY_Y - 2 * WALL         # 70
Z_FLOOR = FLOOR_T                # 3,  interior floor
Z_CEIL = ENV_Z - WALL            # 58, interior ceiling

# --- hanging tabs ---------------------------------------------------------
EAR_NECK = 4.5                   # straight part, wall to eye centre
EAR_W = 11.0
EAR_R = 5.5                      # radius of the eye end
EAR_HOLE = 4.0                   # 3 mm cord knots through comfortably

CORNER_R = 3.0                   # vertical corners
TOP_BREAK = 1.5                  # how much the roof edge is taken off

# --- vent chamber, in the -X/-Y corner ------------------------------------
CH_X, CH_Y = 18.0, 16.0
BAFFLE = 2.0
CH_X0 = -IN_X / 2                # -40, against the -X wall
CH_Y0 = -IN_Y / 2                # -35, against the -Y wall

VENT_H = 2.5                     # outlet slots, both walls
VENT_Y_W = 8.0                   # -Y wall, clear of the corner post
VENT_X_W = 7.0                   # -X wall, likewise
VENT_Z = [38.0, 44.0, 50.0]
VENT_TILT = 30.0                 # degrees, sloping down and outward

# --- floor-to-body screws, into M3 heat-set inserts -----------------------
# 4 mm bore in a 9 mm post: 2.5 mm of wall, which is thinner than one would
# choose and is what the already-printed floor plate allows.
#
# The screw positions are not free. They come from `IN_X/2 - 4` and
# `IN_Y/2 - 4`, i.e. the 8 mm post this box shipped with, and the floor plate
# in hand has its countersinks there. Growing the post therefore has to happen
# *around* a fixed hole, and it runs into two features of that same plate: the
# cell reaches x = 31 and the battery guides reach y = +-26. Nine millimetres
# leaves 0.5 mm to both. Ten would not fit.
#
# So brass into 2.5 mm of wall. Warm the insert properly and do not lean on it;
# if one splits, the way out is a reprinted floor with the holes further in,
# which buys 3.5 mm.
BOSS_D, BOSS_PILOT, BOSS_H = 9.0, 4.0, 14.0
# Deliberately not derived from BOSS_D: the printed plate fixes these.
BOSS_XY = [(sx * (IN_X / 2 - 4.0), sy * (IN_Y / 2 - 4.0))
           for sx in (-1, 1) for sy in (-1, 1)]
SCREW_CLEAR, SCREW_CSK = 3.4, 6.6

# --- bending-beam anchor (M4) ---------------------------------------------
# Interface to the bending-beam clamp. The clamp's own model lived in this
# file until it was retired -- `git show d0df819:models.py` still has it. The
# mating dimensions stay here, because the anchor is meaningless without them.
ANCHOR_PITCH = 15.0              # clamp screw pitch
ANCHOR_HOLE = 4.3
NUT_AF, NUT_T = 7.2, 3.4         # M4 nut across flats, thickness
PAD_X, PAD_Y, PAD_H = 27.0, 14.0, 3.5
FLANGE_X, FLANGE_Y, FLANGE_H = 33.0, 18.0, 2.5
NUT_BOSS_H = 5.0                 # boss inside the box carrying the nuts

# Boards sit on rails this high, which is exactly the nut boss. The anchor
# then lives *under* the ESP instead of fighting it for floor area -- and the
# ESP's 42 mm depth is the one footprint that cannot avoid the centre.
DECK_Z = FLOOR_T + NUT_BOSS_H    # 8

CABLE_D, CABLE_XY = 6.0, (-34.0, 20.0)   # load-cell cable, up through the floor
DRAIN_D, DRAIN_XY = 3.0, (-34.0, 0.0)    # condensate drain

# --- the parts, as measured, plus 2 mm clearance --------------------------
# (footprint x, footprint y, height, centre x, centre y, base z, rail offsets)
# The rail offsets are given rather than derived: the obvious symmetric pair
# put the ESP's front rail straight across the two anchor screws.
# The ESP gets one rail, at the far end. Its second support is the anchor's
# nut boss, which stands at exactly this height and sits inside its footprint.
# A symmetric second rail would have covered the two nut pockets, and the
# nuts have to drop in from above.
ESP = (32.0, 41.0, 37.0, 1.0, -14.5, DECK_Z, (-15.5,))
HX711 = (42.0, 26.0, 32.0, -9.0, 21.0, DECK_Z, (-9.0, 9.0))
RIB, RAIL_W = 2.0, 4.0           # pocket rib, support rail
RIB_H = 6.0                      # how far a rib stands above the rail

# Battery on edge, doubling as the divider between the boards and the +X
# wall. Held by two cable ties, not clamped: a pouch cell swells a little as
# it ages, and a pocket sized to a new one is a press fit on an old one.
CELL_T, CELL_L, CELL_H = 12.0, 67.0, 52.0
CELL_X = 25.0                    # lane centre; cell spans x = 19 .. 31
CELL_RIB_H = 30.0                # side guides, tall enough to hold it upright
CELL_GUIDE_Y = 26.0              # ... but stopping short of the corner posts


def _box(l, w, h, at=(0.0, 0.0, 0.0)):
    """Axis-aligned box, centred in X/Y, sitting on z=at[2]."""
    return (
        cq.Workplane("XY")
        .box(l, w, h, centered=(True, True, False))
        .translate(at)
    )


def _cyl(d, h, at=(0.0, 0.0, 0.0)):
    """Axis-aligned cylinder, centred in X/Y, sitting on z=at[2]."""
    return cq.Workplane("XY").circle(d / 2).extrude(h).translate(at)


def _export(shape, stem):
    """Write <stem>.stl and <stem>.step, both reproducible.

    OpenCASCADE stamps the wall-clock time into the STEP header, so an
    unchanged model would show up as a diff on every run. The meshes are
    tracked, so that churn is not free -- pin the field instead.

    It also numbers each PRODUCT with a counter that runs across the whole
    process, so *adding a part* renumbers every part exported after it. That
    is the same churn wearing a different hat: the four climate/wohnzimmer
    STEPs moved from `translator 7.9 4` to `... 6` when the two beam clamps
    were added ahead of them, with byte-identical meshes. Pin it too.
    """
    exporters.export(shape, str(path_save / (stem + ".stl")))
    step = path_save / (stem + ".step")
    exporters.export(shape, str(step))
    text = re.sub(
        r"(FILE_NAME\('[^']*',')[^']*(')",
        r"\g<1>1970-01-01T00:00:00\g<2>",
        step.read_text(),
        count=1,
    )
    text = re.sub(
        r"(Open CASCADE STEP translator [0-9.]+) [0-9]+",
        r"\g<1>",
        text,
    )
    step.write_text(text)


# ---------------------------------------------------------------------------
# Body
# ---------------------------------------------------------------------------
# Corners are rounded on the source boxes, before anything is cut into them:
# at that point `|Z` selects exactly the four vertical edges and nothing else.
body = _box(BODY_X, BODY_Y, ENV_Z - Z_FLOOR, (0, 0, Z_FLOOR)).edges("|Z").fillet(CORNER_R)
body = body.union(
    _box(ENV_X, ENV_Y, EAVE_H, (0, 0, ENV_Z - EAVE_H)).edges("|Z").fillet(CORNER_R)
)

# Hollow it out. The cavity reaches the bottom of the walls, so the box is
# open downward and the roof stays a single unbroken wall.
body = body.cut(_box(IN_X, IN_Y, Z_CEIL - Z_FLOOR, (0, 0, Z_FLOOR)))

# Hanging tabs, flush with the eave band.
for sx in (-1, 1):
    eye = sx * (ENV_X / 2 + EAR_NECK)
    body = body.union(_box(EAR_NECK, EAR_W, EAVE_H,
                           (sx * (ENV_X / 2 + EAR_NECK / 2), 0, ENV_Z - EAVE_H)))
    body = body.union(
        cq.Workplane("XY").circle(EAR_R).extrude(EAVE_H)
        .translate((eye, 0, ENV_Z - EAVE_H))
    )
    body = body.cut(
        cq.Workplane("XY").circle(EAR_HOLE / 2).extrude(EAVE_H)
        .translate((eye, 0, ENV_Z - EAVE_H))
    )

# No partition wall around the SHT31, deliberately, and it used to be here.
#
# The wall was a baffle keeping the board's own heat out of the sensor's air --
# worth about 0.9 C on the bedroom node, so not nothing. It came out because
# the cable could not be got past it during assembly: first the slot was
# widened, then run full height, and it still fouled. A box that cannot be
# built is worse than one that reads half a degree warm, and the wire has to
# reach the board.
#
# What is left in the corner still helps: the outlet slots are in the two
# walls right beside the sensor and the inlet slots are in the floor beneath
# it, so there is a local draught across it even though the volume is now
# shared. Keep the SHT31 in this corner rather than moving it next to the
# board, and the loss stays at the wall rather than compounding.

# Outlet slots, tilted down and outward so nothing runs in. Two walls, each in
# the stretch the corner post does not stand behind.
for z in VENT_Z:
    body = body.cut(
        cq.Workplane("XY").box(VENT_Y_W, 12.0, VENT_H)
        .rotate((0, 0, 0), (1, 0, 0), VENT_TILT)
        .translate((CH_X0 + 13.0, -BODY_Y / 2 + WALL / 2, z))
    )
    body = body.cut(
        cq.Workplane("XY").box(12.0, VENT_X_W, VENT_H)
        .rotate((0, 0, 0), (0, 1, 0), -VENT_TILT)
        .translate((-BODY_X / 2 + WALL / 2, CH_Y0 + 12.0, z))
    )

# Corner posts for the floor screws.
for (px, py) in BOSS_XY:
    body = body.union(_box(BOSS_D, BOSS_D, BOSS_H, (px, py, Z_FLOOR)))
    body = body.cut(
        cq.Workplane("XY").circle(BOSS_PILOT / 2).extrude(BOSS_H)
        .translate((px, py, Z_FLOOR))
    )

# Relief where the printed floor plate's HX711 rib passes the -X/+Y post.
#
# The post is 9 mm so the insert gets 2.5 mm of wall, and that is 0.5 mm more
# than the plate leaves at this one spot. Taking it back here rather than
# shrinking the post keeps the material where the brass goes: the rib only has
# to slide past, the wall has to survive an insert being melted into it.
rib_x = HX711[3] - (HX711[0] + RIB) / 2
body = body.cut(_box(RIB + 0.6, 14.0, BOSS_H, (rib_x, 30.0, Z_FLOOR)))

# The roof edge gets a chamfer, not a round. This part prints roof-down, so
# that edge is the first layer: a fillet there starts as a knife edge with a
# horizontal tangent and each layer steps outward over air. A 45 degree break
# prints cleanly, softens the same edge, and leaves the eave's underside sharp,
# which is the edge that actually sheds the water.
body = body.faces(">Z").edges().chamfer(TOP_BREAK)

display(body)
_export(body, "terrasse_body")


# ---------------------------------------------------------------------------
# Floor
# ---------------------------------------------------------------------------
floor = _box(BODY_X, BODY_Y, FLOOR_T).edges("|Z").fillet(CORNER_R)

# Countersunk screw holes, heads on the underside. Done while the plate is
# still a plain box, so `<Z` is the plate face and not the anchor pad.
floor = (
    floor.faces("<Z").workplane()
    .pushPoints(BOSS_XY)
    .cskHole(SCREW_CLEAR, SCREW_CSK, 90)
)

# Beam anchor: flange, then the pad that meets the clamp.
floor = floor.union(_box(FLANGE_X, FLANGE_Y, FLANGE_H, (0, 0, -FLANGE_H)))
floor = floor.union(_box(PAD_X, PAD_Y, PAD_H, (0, 0, -FLANGE_H - PAD_H)))
# Boss inside the box, so the anchor screws run through 14 mm of material.
floor = floor.union(_box(PAD_X, PAD_Y, NUT_BOSS_H, (0, 0, FLOOR_T)))

anchor_pts = [(-ANCHOR_PITCH / 2, 0.0), (ANCHOR_PITCH / 2, 0.0)]
anchor_z0 = -FLANGE_H - PAD_H
for (px, py) in anchor_pts:
    floor = floor.cut(
        cq.Workplane("XY").circle(ANCHOR_HOLE / 2)
        .extrude(FLOOR_T + NUT_BOSS_H - anchor_z0)
        .translate((px, py, anchor_z0))
    )
    # Captive nut, dropped in from inside the box.
    floor = floor.cut(
        cq.Workplane("XY").polygon(6, NUT_AF / math.cos(math.radians(30)))
        .extrude(NUT_T)
        .translate((px, py, FLOOR_T + NUT_BOSS_H - NUT_T))
    )

# Load-cell cable, with a collar underneath that sheds water off the lead.
floor = floor.union(
    cq.Workplane("XY").circle(5.0).extrude(4.0)
    .translate((CABLE_XY[0], CABLE_XY[1], -4.0))
)
floor = floor.cut(
    cq.Workplane("XY").circle(CABLE_D / 2).extrude(FLOOR_T + 4.0)
    .translate((CABLE_XY[0], CABLE_XY[1], -4.0))
)

# Condensate drain for the electronics volume.
floor = floor.cut(
    cq.Workplane("XY").circle(DRAIN_D / 2).extrude(FLOOR_T)
    .translate((DRAIN_XY[0], DRAIN_XY[1], 0))
)

# Air inlet, in the leg of the L the sensor card does not stand in.
for sy in (-33.0, -30.5, -28.0):
    floor = floor.cut(_box(8.0, 2.0, FLOOR_T, (CH_X0 + 13.0, sy, 0)))

# Card slot for the SHT31 breakout, standing on edge across the chamber's
# other leg. Held clear of the outer wall: it belongs to the floor, the wall
# to the body, and they have to come apart.
CARD_XY = (CH_X0 + 9.0, CH_Y0 + 12.0)
floor = floor.union(_box(16.0, 6.0, 6.0, (CARD_XY[0], CARD_XY[1], FLOOR_T)))
floor = floor.cut(_box(20.0, 2.0, 5.0, (CARD_XY[0], CARD_XY[1], FLOOR_T + 1.5)))

# Board mounts. Each board gets two rails to sit on at DECK_Z and a rib frame
# to locate it. The rails are what put the anchor's nut boss underneath the
# ESP rather than in its way.
for (fx, fy, fh, cx, cy, bz, rails) in (ESP, HX711):
    for ry in rails:
        floor = floor.union(_box(fx - 2 * RAIL_W, RAIL_W, NUT_BOSS_H,
                                 (cx, cy + ry, FLOOR_T)))
    for sx in (-1, 1):
        floor = floor.union(_box(RIB, fy, NUT_BOSS_H + RIB_H,
                                 (cx + sx * (fx + RIB) / 2, cy, FLOOR_T)))

# One rib between the two boards; the box walls stop them on the other side.
# 41 + 26 + 2 = 69 of the 70 mm available, so there is no room for a rib on
# every side -- the walls do that half of the work.
floor = floor.union(_box(47.0, RIB, NUT_BOSS_H + RIB_H, (-6.5, 7.0, FLOOR_T)))

# Battery lane: two side guides. No cable tie here, unlike the earlier
# version of this box -- the cell now stands 52 mm tall, so a tie would have
# to pass over its top, and on the -X side that lands under the ESP. It is
# captured on all four sides instead: the guides in X, the box walls in Y
# (67 mm cell in a 70 mm interior), the floor below. If it rattles, a strip of
# self-adhesive foam on the guide tops takes up the last 3 mm.
for sx in (-1, 1):
    floor = floor.union(
        _box(RIB, 2 * CELL_GUIDE_Y, CELL_RIB_H,
             (CELL_X + sx * (CELL_T + 0.4 + RIB) / 2, 0, FLOOR_T))
    )

display(floor)
_export(floor, "terrasse_floor")

print("terrasse body  %.1f cm3   floor %.1f cm3" % (
    body.val().Volume() / 1000.0, floor.val().Volume() / 1000.0))


## ===========================================================================
## Terrasse — the bending-beam clamps
## ===========================================================================
##
## Two printed parts, and they are what the floor's anchor pad exists for:
##
##   terrasse_beam_spacer   bolts up to the anchor, carries the beam's fixed end
##   terrasse_beam_hanger   grips the beam's load end, reaches back to the
##                          centre of the box, and carries the wire
##
## An earlier pair of clamps lived in this file and was retired in f858d60;
## `git show d0df819:models.py` still has them. These are not those. The old
## ones put the wire hole 27.5 mm out from their own screws, which is where the
## bar puts it -- off the box's centre line.
##
## Three things decide the shape:
##
##   a) The bar cannot bolt straight to the anchor. Both of its ends carry the
##      same 15 mm screw pitch as the anchor, so one pair of screws cannot do
##      both jobs at once. The spacer offsets the fixed end in X until the two
##      pairs clear each other, and it is the spacer's height that lets the
##      free end deflect instead of fouling the floor.
##   b) **The wire leaves from the far end. That is a requirement, not a
##      preference** -- it is where the feeder has to hang from -- and the rest
##      of the part is dimensioned to satisfy it. Bolts at one end, wire at the
##      other, WIRE_X 61.5 mm away.
##
##      What follows from it is a cantilever in the load path, so the spine
##      rather than the pad is the deep section: RAIL_T is 14 mm, and stiffness
##      goes with depth cubed. That is the constraint being met, and it is why
##      the spine is sized the way it is rather than trimmed to save filament.
##      Deflection here is not a safety question, it is an accuracy one: what
##      bends does not spring back exactly, and the difference shows up as
##      hysteresis in the weight. If readings ever drift between a loaded and
##      an unloaded pan, this section is the first thing to suspect -- deepen
##      RAIL_T before touching anything in the firmware.
##   c) Nothing but the bar may bridge the two clamps. Only the hanger's pad
##      touches the bar; the spine runs RAIL_GAP clear of it for its whole
##      length. Touch anything and the load path goes around the strain gauges:
##      the cell reads a fraction of the weight, or none of it, and it does so
##      quietly.
##
## It also has to be printable, which decides the section. The pad reaches down
## to the rail's underside instead of sitting proud of it, so the part has one
## flat face across all 80 mm and a single 4 mm step on top -- a step *up*,
## overhanging nothing. The only downward faces off the bed are the two
## counterbore ceilings, 112.9 mm² of bridge over a 5.3 mm hole, which is what
## every counterbore printed face-down does. No support anywhere.
##
## The pad ends up 18 mm thick, so the M5 has to span BOLT_BEARING of pad plus
## the bar's thread: M5x16, not M5x12.

# --- the bar, as measured --------------------------------------------------
BEAM_L = 80.0                    # the bar's overall length
BEAM_SPAN = 55.0                 # centre of one screw pair to the other
BEAM_PITCH = 15.0                # screw pitch within a pair; same as ANCHOR_PITCH
BEAM_H = 12.7                    # bar section height, sets the arm's headroom
FIXED_HOLE = 4.3                 # M4 clearance, the end that meets the box
LOAD_HOLE = 5.3                  # M5 clearance, the end that carries the load

# --- the clamps ------------------------------------------------------------
CLAMP_W = 12.0                   # spacer, matching the pad's mating face
CLAMP_EDGE = 5.0                 # material beyond the outermost screw centre
SPACER_H = 10.0
HANGER_W = 18.0                  # hanger is wider: the M5 counterbores need it
BOLT_BEARING = 8.0               # pad material left above the counterbore
RAIL_T, RAIL_GAP = 14.0, 4.0     # spine thickness, and its free air under the bar
WIRE_EDGE = 6.0                  # material beyond the wire hole at the free end
CBORE_D, CBORE_H = 8.0, 4.0      # M4/M5 cap-head counterbore
WIRE_D = 3.4                     # 3 mm wire, plus clearance

# Where the bar's two screw pairs land. The fixed end is offset from the anchor
# so the two screw pairs clear each other; BEAM_DIR then says which way the bar
# runs from there, and everything below follows it. Flip the sign to mirror the
# whole assembly -- it is the only edit that takes.
#
# |FIXED_X| stays at 25 either way: the bar has to run back across the box
# rather than out past its wall, since 80 mm of bar hung off one end would put
# the load 87 mm off centre on a box that is 88 mm wide.
FIXED_X = 25.0
BEAM_DIR = -1                    # -1: bar runs towards -X. +1 mirrors it.
LOAD_X = FIXED_X + BEAM_DIR * BEAM_SPAN

PAD_Z = -(FLANGE_H + PAD_H)      # underside of the anchor pad, -6
SPACER_Z = PAD_Z - SPACER_H      # -16
BEAM_Z = SPACER_Z - BEAM_H       # underside of the bar, -28.7
RAIL_TOP = BEAM_Z - RAIL_GAP     # -32.7, so the spine never touches the bar
RAIL_Z = RAIL_TOP - RAIL_T       # -46.7, and the one face the part prints on

# The pad reaches all the way down to the rail's underside rather than sitting
# proud of it. The tops cannot be flush -- RAIL_GAP is the clearance that keeps
# the spine off the bar -- so the flat face has to be the bottom one. That
# leaves a single 4 mm step, on top, rising towards the pad: a step up prints as
# a step up, with nothing overhanging and no support anywhere on the part.
PAD_T = BEAM_Z - RAIL_Z

# The bar overhangs its screw pairs evenly, which is what makes the hanger 80 mm
# long: it spans the same footprint.
_overhang = (BEAM_L - BEAM_SPAN) / 2
BEAM_X0 = min(FIXED_X, LOAD_X) - _overhang
BEAM_X1 = max(FIXED_X, LOAD_X) + _overhang

_spacer_x0 = min(FIXED_X - BEAM_PITCH / 2, -ANCHOR_PITCH / 2) - CLAMP_EDGE
_spacer_x1 = max(FIXED_X + BEAM_PITCH / 2, ANCHOR_PITCH / 2) + CLAMP_EDGE

# ---------------------------------------------------------------------------
# Spacer — anchor above, bar below
# ---------------------------------------------------------------------------
spacer = _box(_spacer_x1 - _spacer_x0, CLAMP_W, SPACER_H,
              ((_spacer_x0 + _spacer_x1) / 2, 0, SPACER_Z))

# Up into the floor's captive nuts: head recessed in the underside.
for sx in (-1, 1):
    px = sx * ANCHOR_PITCH / 2
    spacer = spacer.cut(_cyl(ANCHOR_HOLE, SPACER_H, (px, 0, SPACER_Z)))
    spacer = spacer.cut(_cyl(CBORE_D, CBORE_H, (px, 0, SPACER_Z)))

# Down into the bar's own threads. Both sit outside the pad's 27 mm footprint,
# so their heads have somewhere to go.
for sx in (-1, 1):
    px = FIXED_X + sx * BEAM_PITCH / 2
    spacer = spacer.cut(_cyl(FIXED_HOLE, SPACER_H, (px, 0, SPACER_Z)))
    spacer = spacer.cut(_cyl(CBORE_D, CBORE_H, (px, 0, PAD_Z - CBORE_H)))

display(spacer)
_export(spacer, "terrasse_beam_spacer")

# ---------------------------------------------------------------------------
# Hanger — the wire hangs between the bolts, on a pad that carries everything
# ---------------------------------------------------------------------------
# The pad is the whole load path: wire in the middle, two bolts either side,
# 15 mm apart. Nothing between them bends, so nothing is lost there. The spine
# behind it reaches the bar's full 80 mm and carries no load at all.
_pad_x0 = LOAD_X - BEAM_PITCH / 2 - CLAMP_EDGE
_pad_x1 = LOAD_X + BEAM_PITCH / 2 + CLAMP_EDGE

# Bolts at one end, wire at the other. The spine spans between them, and it is
# the whole load path -- so it is the deep section, not the pad.
if BEAM_DIR < 0:
    _rail_x0, _rail_x1 = _pad_x1 - 2.0, BEAM_X1
    WIRE_X = BEAM_X1 - WIRE_EDGE
else:
    _rail_x0, _rail_x1 = BEAM_X0, _pad_x0 + 2.0
    WIRE_X = BEAM_X0 + WIRE_EDGE

hanger = _box(_pad_x1 - _pad_x0, HANGER_W, PAD_T,
              ((_pad_x0 + _pad_x1) / 2, 0, RAIL_Z))
hanger = hanger.union(_box(_rail_x1 - _rail_x0, HANGER_W, RAIL_T,
                           ((_rail_x0 + _rail_x1) / 2, 0, RAIL_Z)))

# Up into the bar's load end. The counterbore is sunk deep enough that an
# ordinary M5 still reaches the thread through an 18 mm pad, leaving
# BOLT_BEARING of material under the bar.
for sx in (-1, 1):
    px = LOAD_X + sx * BEAM_PITCH / 2
    hanger = hanger.cut(_cyl(LOAD_HOLE, PAD_T, (px, 0, RAIL_Z)))
    hanger = hanger.cut(_cyl(CBORE_D + 2.0, PAD_T - BOLT_BEARING, (px, 0, RAIL_Z)))

# The wire, at the free end, on the same centre line as the bolts.
hanger = hanger.cut(_cyl(WIRE_D, RAIL_T, (WIRE_X, 0.0, RAIL_Z)))

display(hanger)
_export(hanger, "terrasse_beam_hanger")

print("terrasse spacer %.1f cm3   hanger %.1f cm3 (%.0f mm long)   "
      "spine clears bar by %.1f mm" % (
          spacer.val().Volume() / 1000.0, hanger.val().Volume() / 1000.0,
          BEAM_X1 - BEAM_X0, BEAM_Z - RAIL_TOP))
print("           wire at x=%.1f, %.1f mm from the bolts at x=%.1f; "
      "one flat face, %.0f mm step on top"
      % (WIRE_X, abs(WIRE_X - LOAD_X), LOAD_X, PAD_T - RAIL_T))
print("           M5 must reach %.0f mm of pad plus the bar's thread"
      % BOLT_BEARING)



## ===========================================================================
## Wohnzimmer — indoor housing for the air-quality node
## ===========================================================================
##
## Two printed parts:
##
##   wohnzimmer_tray   floor, walls and the three compartments
##   wohnzimmer_lid    flat cover, four screws
##
## Indoors, so nothing here is about rain. The shape is driven by two
## constraints the commissioning notes already state:
##
##   "keep the SHT31-D away from the board [...] a board-warmed SHT31 reports
##    a relative humidity that is too low, the correction then subtracts too
##    little, and the error lands in the PM figures"
##   "a sealed enclosure would have it measuring the enclosure"
##
## Hence three compartments in a row, divided by full-height baffles that also
## carry the wz_lid across its span:
##
##   -X  sensor chamber   SHT31 + SCD41, vented on three sides, ~90 mm of
##                        still air away from the board. Both sensors here
##                        want room air and neither runs a fan.
##       SDS011 bay       a pocket the module drops into. Its intake is tubed
##                        to a stub in the +Y wall so it draws room air, not
##                        its own exhaust; the exhaust leaves through the -Y
##                        wall, 84 mm away on the far side of the module.
##   +X  board bay        a fitted pocket, cable out through the +X wall.
##
## A 10 mm service strip runs along +Y past the SDS011 for the intake tube and
## for the sensor wiring, which has to cross the bay to reach the board. The
## baffles are notched at floor level to let it through.
##
## Print both parts flat on the plate, wz_tray floor down. Slots are vertical
## cuts in vertical walls, so only their tops bridge -- no support needed.

WZ_X, WZ_Y, WZ_Z = 145.0, 89.0, 42.0
WZ_WALL = 2.5
WZ_FLOOR = 3.0
WZ_LID = 3.0

WZ_IN_X = WZ_X - 2 * WZ_WALL      # 140
WZ_IN_Y = WZ_Y - 2 * WZ_WALL      # 84
WZ_IN_H = WZ_Z - WZ_FLOOR - WZ_LID  # 26
WZ_TOP = WZ_FLOOR + WZ_IN_H       # 29, where the wz_lid lands

WZ_BAF = 2.0

# Component envelopes. Measured values go here; everything else follows.
SDS_XY = 73.5                     # 71 x 70 module, pocket kept SQUARE so it
                                  # can be turned to any of four orientations
                                  # -- which edge carries the intake nozzle
                                  # differs between units, and the tube has to
                                  # reach the stub.
SDS_SERVICE = 10.0                # strip along +Y for tube and wiring
SENS_X = 18.0                     # sensor chamber depth
BOARD_X, BOARD_Y = 34.0, 56.0     # 33 x 55 board plus clearance
BOARD_RIB = 8.0                   # pocket rib height

# Compartment boundaries in X, left to right.
WZ_X0 = -WZ_IN_X / 2              # -70
SENS_X1 = WZ_X0 + SENS_X          # -52
SDS_X0 = SENS_X1 + WZ_BAF         # -50
SDS_X1 = SDS_X0 + SDS_XY          # 23.5
BOARD_X0 = SDS_X1 + WZ_BAF        # 25.5

WZ_Y1 = WZ_IN_Y / 2               # 42
SDS_RIB_Y = WZ_Y1 - SDS_SERVICE - WZ_BAF   # 30, module stops here

WZ_POST = 8.0
# 3.5 mm for a heat-set insert, not a self-tapping pilot. That leaves 2.25 mm
# of post wall around it -- brass expands as it goes in, so check it against
# the insert you actually have before printing four of them.
WZ_PILOT, WZ_CLEAR, WZ_CSK = 3.5, 3.4, 6.6
WZ_POST_XY = [(sx * (WZ_IN_X / 2 - WZ_POST / 2), sy * (WZ_IN_Y / 2 - WZ_POST / 2))
              for sx in (-1, 1) for sy in (-1, 1)]

INTAKE_OD, INTAKE_ID, INTAKE_L = 6.0, 4.0, 8.0   # stub mimics the SDS011 nozzle
SLOT_W = 2.5                                      # every vent slot


def _slots(shape, n, pitch, size, at, axis="z"):
    """Cut `n` slots of `size` (l, w, h), stepped by `pitch` along `axis`."""
    for i in range(n):
        d = (i - (n - 1) / 2) * pitch
        off = {"x": (d, 0, 0), "y": (0, d, 0), "z": (0, 0, d)}[axis]
        shape = shape.cut(_box(*size, tuple(a + b for a, b in zip(at, off))))
    return shape


# ---------------------------------------------------------------------------
# Tray
# ---------------------------------------------------------------------------
wz_tray = _box(WZ_X, WZ_Y, WZ_TOP).edges("|Z").fillet(CORNER_R)
wz_tray = wz_tray.cut(_box(WZ_IN_X, WZ_IN_Y, WZ_IN_H, (0, 0, WZ_FLOOR)))

# Baffles. Full height: they separate the three air volumes and they are what
# keeps a 145 mm wz_lid from sagging between its four corner screws.
for bx in (SENS_X1 + WZ_BAF / 2, SDS_X1 + WZ_BAF / 2):
    wz_tray = wz_tray.union(_box(WZ_BAF, WZ_IN_Y, WZ_IN_H, (bx, 0, WZ_FLOOR)))
    # Full-height slot inside the service strip, for the sensor wiring: the
    # wire drops in from above instead of being threaded through a window.
    # Seal it after routing -- see the note on the terrasse baffle.
    wz_tray = wz_tray.cut(
        _box(3 * WZ_BAF, 8.0, WZ_IN_H, (bx, WZ_Y1 - SDS_SERVICE / 2, WZ_FLOOR))
    )

# Rib that stops the SDS011 short of the service strip.
wz_tray = wz_tray.union(_box(SDS_XY, WZ_BAF, 6.0,
                       ((SDS_X0 + SDS_X1) / 2, SDS_RIB_Y + WZ_BAF / 2, WZ_FLOOR)))

# Intake stub in the +Y wall: push a short silicone tube from the module's
# nozzle onto this, and the fan draws room air instead of the bay's own
# exhaust. Without it the sensor slowly re-measures what it just measured.
stub_c = ((SDS_X0 + SDS_X1) / 2, WZ_Y1 + WZ_WALL - INTAKE_L / 2, WZ_FLOOR + 9.0)
wz_tray = wz_tray.union(
    cq.Workplane("XZ").circle(INTAKE_OD / 2).extrude(INTAKE_L)
    .translate((stub_c[0], WZ_Y1 + WZ_WALL, stub_c[2]))
)
wz_tray = wz_tray.cut(
    cq.Workplane("XZ").circle(INTAKE_ID / 2).extrude(INTAKE_L + WZ_WALL + 2)
    .translate((stub_c[0], WZ_Y1 + WZ_WALL + 1, stub_c[2]))
)

# Exhaust, -Y wall, the full length of the bay and as far from the intake as
# the box allows. The top row sits where it does because the walls grew 10 mm:
# a chimney is driven by the height between inlet and outlet, so the extra
# height is worth spending on one more row rather than on dead air.
for z in (8.0, 13.0, 18.0, 23.0):
    wz_tray = _slots(wz_tray, 4, 17.0, (14.0, 3 * WZ_WALL, SLOT_W),
                  ((SDS_X0 + SDS_X1) / 2, -WZ_Y1, WZ_FLOOR + z), axis="x")

# Sensor chamber: vented on the -X end and both long walls, so it sees room
# air by convection alone. No fan reaches in here.
for z in (7.0, 12.0, 17.0, 22.0):
    wz_tray = _slots(wz_tray, 2, 34.0, (3 * WZ_WALL, 26.0, SLOT_W),
                  (-WZ_IN_X / 2, 0, WZ_FLOOR + z), axis="y")
    # Kept inboard of the corner posts: a slot cut across one would open the
    # wall onto solid plastic and vent nothing.
    for sy in (-1, 1):
        wz_tray = wz_tray.cut(_box(8.0, 3 * WZ_WALL, SLOT_W,
                             (WZ_X0 + WZ_POST + 5.0, sy * WZ_Y1, WZ_FLOOR + z)))

# Card slot for the SHT31, standing on edge at the far end of the chamber
# from the SCD41, which shares the compartment and does run slightly warm.
wz_tray = wz_tray.union(_box(6.0, 18.0, 6.0, (WZ_X0 + SENS_X / 2, -22.0, WZ_FLOOR)))
wz_tray = wz_tray.cut(_box(2.0, 22.0, 5.0, (WZ_X0 + SENS_X / 2, -22.0, WZ_FLOOR + 1.5)))

# Board pocket, and the cable out through the +X wall.
wz_tray = wz_tray.union(_box(WZ_BAF, BOARD_Y, BOARD_RIB,
                       (BOARD_X0 + BOARD_X + WZ_BAF / 2, 0, WZ_FLOOR)))
for sy in (-1, 1):
    wz_tray = wz_tray.union(_box(BOARD_X + WZ_BAF, WZ_BAF, BOARD_RIB,
                           (BOARD_X0 + (BOARD_X + WZ_BAF) / 2,
                            sy * (BOARD_Y + WZ_BAF) / 2, WZ_FLOOR)))
wz_tray = wz_tray.cut(_box(18.0, 14.0, 10.0, (WZ_IN_X / 2, 0, WZ_FLOOR)))
for z in (16.0, 20.0):
    wz_tray = wz_tray.cut(_box(3 * WZ_WALL, 26.0, SLOT_W, (WZ_IN_X / 2, 0, WZ_FLOOR + z)))

# Corner posts for the wz_lid.
for (px, py) in WZ_POST_XY:
    wz_tray = wz_tray.union(_box(WZ_POST, WZ_POST, WZ_IN_H, (px, py, WZ_FLOOR)))
    wz_tray = wz_tray.cut(
        cq.Workplane("XY").circle(WZ_PILOT / 2).extrude(WZ_IN_H)
        .translate((px, py, WZ_FLOOR))
    )

display(wz_tray)
_export(wz_tray, "wohnzimmer_tray")


# ---------------------------------------------------------------------------
# Lid
# ---------------------------------------------------------------------------
# The wz_lid prints underside-down, so its top edge is the LAST thing laid down
# and each layer of the round is smaller than the one below it. Free to print,
# unlike the same edge on the terrasse roof.
wz_lid = _box(WZ_X, WZ_Y, WZ_LID).edges("|Z").fillet(CORNER_R)
wz_lid = wz_lid.faces(">Z").edges().fillet(TOP_BREAK)
wz_lid = (
    wz_lid.faces(">Z").workplane()
    .pushPoints(WZ_POST_XY)
    .cskHole(WZ_CLEAR, WZ_CSK, 90)
)

display(wz_lid)
_export(wz_lid, "wohnzimmer_lid")

print("wohnzimmer wz_tray %.1f cm3  wz_lid %.1f cm3" % (
    wz_tray.val().Volume() / 1000.0, wz_lid.val().Volume() / 1000.0))


## ===========================================================================
## Küche / Bad — indoor housing for the plain climate nodes
## ===========================================================================
##
## One design, printed twice: node.rs describes `kueche` as "the same build as
## BAD", and the contents are the same too -- a XIAO and an SHT31 on jumper
## wires, nothing else.
##
## Small, but the same rule still applies: the SHT31 does not share air with
## the board. Two compartments, a baffle between them, and the sensor end
## vented on three sides. A node whose only job is temperature and humidity
## has nothing to report if it reports the inside of its own box.
##
## The board bay is vented too, and that took a second pass to get right. The
## first version left it sealed: three rows of slots at the sensor end, none at
## the board end, and a baffle with a full-height notch in it for the wires.
## Sealed is the wrong word for what that is -- the notch is 168 mm2 of opening,
## and with no other way out, every watt the board made left through it and
## crossed the sensor chamber on the way to the only vents in the box. The
## baffle was not separating two volumes, it was aiming one at the other. In
## service that read about 2.5 C warm, and the humidity with it: the sensor
## measures the air it is in, so heating the air by 2.5 C without adding water
## to it drops the reported RH by about seven points.
##
## So the board bay gets its own chimney -- inlet low, two outlets high, on both
## long walls -- and the notch stops being the path of least resistance. The
## notch itself is left alone deliberately; see the comment on it below.

KL_X, KL_Y, KL_Z = 65.0, 39.0, 30.0
KL_WALL, KL_FLOOR, KL_LID = 2.5, 3.0, 3.0

KL_IN_X = KL_X - 2 * KL_WALL      # 60
KL_IN_Y = KL_Y - 2 * KL_WALL      # 34
KL_IN_H = KL_Z - KL_FLOOR - KL_LID  # 24
KL_TOP = KL_FLOOR + KL_IN_H       # 27

KL_BAF = 2.0
KL_SENS_X = 22.0                  # sensor chamber depth
KL_POST = 8.0
# 3.5 mm for a heat-set M3 insert, matching WZ_PILOT rather than the 4.0 this
# briefly had: the brass displaces plastic as it melts in, so the hole wants to
# be under the insert's outer diameter, not equal to it. That also puts 2.25 mm
# of post wall back around it instead of 2.0, which matters here because
# KL_POST cannot grow -- the screw centres are pinned by KL_POST_XY.
KL_PILOT = 3.5
# Measured, not derived -- the same reason BOSS_XY is pinned on the terrace box.
# Flush against the inner walls the centres come out at (26.0, 13.0), and a test
# fit of the printed pair showed the lid would not sit down: the holes had to
# move 1.0 mm further apart along the length and 1.5 mm across the width. That
# is 5.7 % on the short axis, far too much for PLA shrinkage (~0.3 %), so it is
# not a tolerance to be dialled out -- it is measured and stays measured.
#
# The posts now overlap the side walls by 0.5 / 0.75 mm. That is harmless, they
# are unioned into them; what it costs is lid material, 2.7 mm between the
# countersink and the long edge and 2.45 mm on the short one, down from 3.2.
KL_POST_XY = [(sx * 26.5, sy * 13.75) for sx in (-1, 1) for sy in (-1, 1)]

KL_X0 = -KL_IN_X / 2              # -30
KL_SENS_X1 = KL_X0 + KL_SENS_X    # -8
KL_BOARD_X0 = KL_SENS_X1 + KL_BAF  # -6

# XIAO ESP32-C3, 21 x 17.5 mm, lying flat on the kl_tray floor.
XIAO_X, XIAO_Y = 22.5, 19.0
XIAO_X0 = 2.5                     # leaves 8.5 mm of slack space for the
                                  # jumper wires between baffle and board
XIAO_RIB = 4.0
USB_W, USB_H = 15.0, 9.0

kl_tray = _box(KL_X, KL_Y, KL_TOP).edges("|Z").fillet(CORNER_R)
kl_tray = kl_tray.cut(_box(KL_IN_X, KL_IN_Y, KL_IN_H, (0, 0, KL_FLOOR)))

# Baffle, with a full-height notch for the jumper wires.
#
# The comment here used to claim the notch was "at floor level" and "kept small
# -- for four wires, not for air", and the code beneath it has always cut the
# full 24 mm. Both halves of that were wrong, and the second one was the
# expensive half: at 7 x 24 it is the largest single opening in the box.
#
# It stays full height anyway. The terrasse box lost its baffle entirely over
# exactly this -- "first the slot was widened, then run full height, and it
# still fouled" -- because a 4-way jumper housing has to drop in from above
# rather than thread through a window, and a box that cannot be assembled is
# worse than one that reads warm. The answer is not to close this, it is to
# stop the board bay needing it: with the vents below, air leaves on the board's
# own side of the baffle and this carries wires again instead of exhaust.
kl_tray = kl_tray.union(_box(KL_BAF, KL_IN_Y, KL_IN_H,
                       (KL_SENS_X1 + KL_BAF / 2, 0, KL_FLOOR)))
kl_tray = kl_tray.cut(
    _box(3 * KL_BAF, 7.0, KL_IN_H, (KL_SENS_X1 + KL_BAF / 2, 0, KL_FLOOR))
)

# Sensor chamber: -X end and both long walls, the vents kept inboard of the
# corner posts so they open onto air rather than onto a post.
for z in (6.0, 11.0, 16.0):
    kl_tray = kl_tray.cut(_box(3 * KL_WALL, 20.0, SLOT_W, (KL_X0, 0, KL_FLOOR + z)))
    for sy in (-1, 1):
        kl_tray = kl_tray.cut(_box(8.0, 3 * KL_WALL, SLOT_W,
                             (KL_X0 + KL_POST + 5.0, sy * KL_IN_Y / 2, KL_FLOOR + z)))

# Card slot for the SHT31 breakout.
kl_tray = kl_tray.union(_box(6.0, 18.0, 6.0, (KL_X0 + KL_SENS_X / 2, 0, KL_FLOOR)))
kl_tray = kl_tray.cut(_box(2.0, 22.0, 5.0, (KL_X0 + KL_SENS_X / 2, 0, KL_FLOOR + 1.5)))

# XIAO pocket, USB-C end toward the +X wall.
kl_tray = kl_tray.union(_box(KL_BAF, XIAO_Y + 2 * KL_BAF, XIAO_RIB,
                       (XIAO_X0 - KL_BAF / 2, 0, KL_FLOOR)))
kl_tray = kl_tray.union(_box(KL_BAF, XIAO_Y + 2 * KL_BAF, XIAO_RIB,
                       (XIAO_X0 + XIAO_X + KL_BAF / 2, 0, KL_FLOOR)))
for sy in (-1, 1):
    kl_tray = kl_tray.union(_box(XIAO_X + 2 * KL_BAF, KL_BAF, XIAO_RIB,
                           (XIAO_X0 + XIAO_X / 2, sy * (XIAO_Y + KL_BAF) / 2, KL_FLOOR)))

# Board bay: the chimney that keeps the board's heat on the board's side of the
# baffle. Inlet just above the corral ribs, two outlets high where the warm air
# actually is. Both long walls, so it is a cross-draught and not one hole.
#
# Walls only, no slots in the kl_lid -- unlike the schlafzimmer box, which has
# them because its SCD41 needs room air and not box air. Nothing in here needs
# room air; it only needs its heat gone, and a high wall slot does that without
# leaving an opening above the board. These two live in a kitchen and a
# bathroom, where what falls from above is grease and condensate.
#
# Kept inboard of the +X corner posts (x >= 22.5) and clear of the baffle, so
# every slot opens onto air rather than onto a post.
KL_BOARD_VENT_X = 11.0
KL_BOARD_VENT_L = 16.0
for z in (5.0, 13.0, 19.0):
    for sy in (-1, 1):
        kl_tray = kl_tray.cut(_box(KL_BOARD_VENT_L, 3 * KL_WALL, SLOT_W,
                             (KL_BOARD_VENT_X, sy * KL_IN_Y / 2, KL_FLOOR + z)))

# Cable out. Sized for a USB-C plug's overmould, not just the connector.
kl_tray = kl_tray.cut(_box(3 * KL_WALL, USB_W, USB_H, (KL_IN_X / 2, 0, KL_FLOOR + 0.5)))

for (px, py) in KL_POST_XY:
    kl_tray = kl_tray.union(_box(KL_POST, KL_POST, KL_IN_H, (px, py, KL_FLOOR)))
    kl_tray = kl_tray.cut(
        cq.Workplane("XY").circle(KL_PILOT / 2).extrude(KL_IN_H)
        .translate((px, py, KL_FLOOR))
    )

display(kl_tray)
_export(kl_tray, "climate_tray")

kl_lid = _box(KL_X, KL_Y, KL_LID).edges("|Z").fillet(CORNER_R)
kl_lid = kl_lid.faces(">Z").edges().fillet(TOP_BREAK)
kl_lid = (
    kl_lid.faces(">Z").workplane()
    .pushPoints(KL_POST_XY)
    .cskHole(WZ_CLEAR, WZ_CSK, 90)
)

display(kl_lid)
_export(kl_lid, "climate_lid")

print("climate kl_tray %.1f cm3  kl_lid %.1f cm3" % (
    kl_tray.val().Volume() / 1000.0, kl_lid.val().Volume() / 1000.0))


## ===========================================================================
## Schlafzimmer — indoor housing for the CO2 node
## ===========================================================================
##
## Two printed parts:
##
##   schlafzimmer_tray   floor, walls and the two compartments
##   schlafzimmer_lid    flat cover, four screws, vented over the stack
##
## Same two-compartment idea as the Kueche/Bad box, but the contents are not
## the same shape at all. Here the SCD41 breakout is *stacked on the XIAO* --
## one 60 x 20 x 35 assembly, measured with the jumper headers on top -- and
## only the SHT31-D hangs off it on wires. So the long axis is set by that
## stack, not by three compartments in a row.
##
## What the stack costs, and why it is accepted: `wiring.md` asks for "both
## sensors in the same air, both away from the board", and a stacked SCD41 is
## by definition neither. That is the hardware in hand, and it is not as bad
## as it sounds -- the offset calibration is a *difference* of two
## temperatures, so a constant gap between the two chambers is exactly what
## the 4 C offset absorbs. What a fixed offset cannot absorb is a *varying*
## one, so the design spends its effort on getting the board's heat out of the
## box rather than on pretending the two sensors share air:
##
##   - the stack bay is vented on both long walls, low for the inlet and twice
##     again at 26 and 32 mm, which is where the SCD41 rides on top of the
##     stack. Those upper rows are the cross-draught over the sensor itself.
##   - the lid is slotted over the bay. That is the chimney's outlet, and it
##     is also the answer to "not inside a sealed enclosure": the CO2 sensor
##     needs room air, not box air, and 6 mm of still plastic above it would
##     have given it the second.
##   - the lid slots run along X, not across it. Both are equally open; only
##     one leaves the material between them running the full 108 mm, and a
##     lid this long with screws only in its corners needs that.
##   - the SHT31 keeps its own vented chamber behind a full-height baffle, as
##     in every other box in this file. It is the reference thermometer; it
##     has no business downwind of the stack.
##
## Print both parts flat on the plate, tray floor down.

SZ_X, SZ_Y, SZ_Z = 108.0, 40.0, 44.0
SZ_WALL, SZ_FLOOR, SZ_LID = 2.5, 3.0, 3.0

SZ_IN_X = SZ_X - 2 * SZ_WALL        # 103
SZ_IN_Y = SZ_Y - 2 * SZ_WALL        # 35
SZ_IN_H = SZ_Z - SZ_FLOOR - SZ_LID  # 38, i.e. 3 mm over the stack
SZ_TOP = SZ_FLOOR + SZ_IN_H         # 41, where the lid lands

SZ_BAF = 2.0
SZ_SENS_X = 22.0                    # sensor chamber depth, as on the KL box

# The stack, measured (60 x 20 x 35) plus clearance. Height is the one that
# must not be trimmed: it is what decides SZ_Z, and the jumper headers on top
# are already counted in the 35.
STACK_X, STACK_Y, STACK_Z = 62.0, 23.0, 35.0
SZ_SLACK = 8.0                      # wire loop between baffle and stack
SZ_RIB = 6.0                        # corral around the lower board

SZ_X0 = -SZ_IN_X / 2                    # -51.5
SZ_SENS_X1 = SZ_X0 + SZ_SENS_X          # -29.5
SZ_STACK_X0 = SZ_SENS_X1 + SZ_BAF + SZ_SLACK   # -19.5
SZ_STACK_X1 = SZ_STACK_X0 + STACK_X            # 42.5
SZ_STACK_XC = (SZ_STACK_X0 + SZ_STACK_X1) / 2  # 11.5

SZ_POST = 8.0
# 3.5 mm as specified for M2.5 inserts. That is right for the common
# M2.5 x 4.0 OD insert and 0.5 mm too wide for a 3.5 OD one -- brass has to
# displace plastic to hold, so check the insert you actually have against the
# hole before melting four of them in.
SZ_PILOT = 3.5
# M2.5 countersunk: 2.9 clearance, head 4.7 plus a lip.
SZ_CLEAR, SZ_CSK = 2.9, 5.7
SZ_POST_XY = [(sx * (SZ_IN_X / 2 - SZ_POST / 2), sy * (SZ_IN_Y / 2 - SZ_POST / 2))
              for sx in (-1, 1) for sy in (-1, 1)]
# Posts reach x = +-43.5 inboard, which is the 1 mm the stack bay stops short
# of. The USB plug then leaves between the two +X posts, 19 mm apart.

# ---------------------------------------------------------------------------
# Tray
# ---------------------------------------------------------------------------
sz_tray = _box(SZ_X, SZ_Y, SZ_TOP).edges("|Z").fillet(CORNER_R)
sz_tray = sz_tray.cut(_box(SZ_IN_X, SZ_IN_Y, SZ_IN_H, (0, 0, SZ_FLOOR)))

# Baffle, with a full-height notch for the four wires to the SHT31.
sz_tray = sz_tray.union(_box(SZ_BAF, SZ_IN_Y, SZ_IN_H,
                       (SZ_SENS_X1 + SZ_BAF / 2, 0, SZ_FLOOR)))
sz_tray = sz_tray.cut(
    _box(3 * SZ_BAF, 8.0, SZ_IN_H, (SZ_SENS_X1 + SZ_BAF / 2, 0, SZ_FLOOR))
)

# Sensor chamber: -X end and both long walls, kept inboard of the corner posts
# so the slots open onto air rather than onto a post.
for z in (6.0, 13.0, 20.0, 27.0, 33.0):
    sz_tray = sz_tray.cut(_box(3 * SZ_WALL, 18.0, SLOT_W, (SZ_X0, 0, SZ_FLOOR + z)))
    for sy in (-1, 1):
        sz_tray = sz_tray.cut(_box(10.0, 3 * SZ_WALL, SLOT_W,
                              (-38.0, sy * SZ_IN_Y / 2, SZ_FLOOR + z)))

# Card slot for the SHT31 breakout, standing on edge across the chamber.
sz_tray = sz_tray.union(_box(6.0, 18.0, 6.0, (SZ_X0 + SZ_SENS_X / 2, 0, SZ_FLOOR)))
sz_tray = sz_tray.cut(_box(2.0, 22.0, 5.0, (SZ_X0 + SZ_SENS_X / 2, 0, SZ_FLOOR + 1.5)))

# Stack bay: inlet low, outlets at the two heights the SCD41 rides at.
for z in (5.0, 26.0, 32.0):
    for sy in (-1, 1):
        sz_tray = _slots(sz_tray, 3, 20.0, (16.0, 3 * SZ_WALL, SLOT_W),
                         (SZ_STACK_XC, sy * SZ_IN_Y / 2, SZ_FLOOR + z), axis="x")

# Corral for the lower board. Closed on three sides; the +X end is two stubs
# with a USB_W gap between them, because a full rib there would stand in front
# of the USB-C plug.
sz_tray = sz_tray.union(_box(SZ_BAF, STACK_Y + 2 * SZ_BAF, SZ_RIB,
                       (SZ_STACK_X0 - SZ_BAF / 2, 0, SZ_FLOOR)))
for sy in (-1, 1):
    sz_tray = sz_tray.union(_box(STACK_X + 2 * SZ_BAF, SZ_BAF, SZ_RIB,
                           (SZ_STACK_XC, sy * (STACK_Y + SZ_BAF) / 2, SZ_FLOOR)))
    sz_tray = sz_tray.union(_box(SZ_BAF, (STACK_Y - USB_W) / 2, SZ_RIB,
                           (SZ_STACK_X1 + SZ_BAF / 2,
                            sy * (USB_W + (STACK_Y - USB_W) / 2) / 2, SZ_FLOOR)))

# Cable out, sized for a USB-C plug's overmould.
sz_tray = sz_tray.cut(_box(3 * SZ_WALL, USB_W, USB_H, (SZ_IN_X / 2, 0, SZ_FLOOR + 0.5)))

for (px, py) in SZ_POST_XY:
    sz_tray = sz_tray.union(_box(SZ_POST, SZ_POST, SZ_IN_H, (px, py, SZ_FLOOR)))
    sz_tray = sz_tray.cut(
        cq.Workplane("XY").circle(SZ_PILOT / 2).extrude(SZ_IN_H)
        .translate((px, py, SZ_FLOOR))
    )

display(sz_tray)
_export(sz_tray, "schlafzimmer_tray")

# ---------------------------------------------------------------------------
# Lid
# ---------------------------------------------------------------------------
# Edge break first: once the vent slots are cut, `>Z` edges are no longer just
# the outline, and a 1.5 mm fillet on the 3 mm web between two slots is not a
# fillet, it is a failed kernel call.
sz_lid = _box(SZ_X, SZ_Y, SZ_LID).edges("|Z").fillet(CORNER_R)
sz_lid = sz_lid.faces(">Z").edges().fillet(TOP_BREAK)
sz_lid = _slots(sz_lid, 5, 6.0, (40.0, 3.0, SZ_LID + 2),
                (SZ_STACK_XC, 0, -1.0), axis="y")
sz_lid = (
    sz_lid.faces(">Z").workplane()
    .pushPoints(SZ_POST_XY)
    .cskHole(SZ_CLEAR, SZ_CSK, 90)
)

display(sz_lid)
_export(sz_lid, "schlafzimmer_lid")

print("schlafzimmer sz_tray %.1f cm3  sz_lid %.1f cm3" % (
    sz_tray.val().Volume() / 1000.0, sz_lid.val().Volume() / 1000.0))
