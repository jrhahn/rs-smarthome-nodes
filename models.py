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
## Four printed parts:
##
##   terrasse_body          walls + closed top. Open at the BOTTOM.
##   terrasse_floor         bottom plate. Carries every mount, and nothing at
##                          all on its underside -- see terrasse_anchor.
##   terrasse_charger_tray  shelf over the boards for the solar charger, added
##                          2026-09-18. Screws to three columns on the floor.
##   terrasse_anchor        the pad the beam clamp bolts to, split off the floor
##                          plate the same day so that plate prints flat.
##
## Print orientation, which is what several of the decisions in here are about:
##
##   body   ROOF DOWN     every feature vertical or stepping inward
##   floor  PLATE DOWN    underside is one flat face, ribs and columns stand up
##   tray   FLAT          either way
##   anchor PAD DOWN      every face flat or vertical
##
## None of the four needs support.
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
##   c) beam anchor            A pad matching the bending-beam clamp (25 x 12
##      face, two screws 15 mm apart), its own part since 2026-09-18 so this
##      plate prints flat. Two M4 x 25 countersunk screws run DOWN from inside
##      the box, through plate and anchor, into nuts in the spacer's underside.
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

# --- the solar charger, and what it costs in height ------------------------
# Soldered CN3791 MPPT board (SKU 333136), 54 x 38 mm. It arrives after the box
# was designed and there is no spare volume on the floor: a packing search put
# the smallest interior taking the existing four parts at 76 x 66 x 52 against
# the 80 x 70 x 55 there is. So it goes *above* the two boards, on a tray, in
# the band their 37 mm leaves under the roof.
#
# CHARGER_H is the measured height of the *populated* board -- connectors, not
# PCB -- and it is the one number that decides whether this box grows. Up to
# 9.5 mm it fits under the existing roof and only the floor plate is reprinted.
# Beyond that ENV_Z follows it automatically, and the body is reprinted too.
CHARGER_X, CHARGER_Y = 38.0, 54.0
# Mounting holes, read off the manufacturer's PCB file: four Ø3.2 at 48 x 32,
# i.e. 3 mm in from each edge of the 54 x 38 board. The 48 runs along its long
# side, so on the tray that is the Y spacing.
CHARGER_HOLES = (32.0, 48.0)     # x pitch, y pitch
# The board stands on four sockets rather than flat on the tray, which is what
# lets an M3 insert live in them -- 2.5 mm of tray never could -- and which the
# board needs anyway: its screw terminals' solder tails stand proud underneath,
# so a board laid flat would rest on its own joints.
CHARGER_BOSS_D, CHARGER_BOSS_H = 8.0, 4.5
CHARGER_BORE, CHARGER_BORE_H = 3.5, 5.0
# Measured 2026-09-18 on the populated board, 2 mm of headroom already in it.
# It does not fit the 13 mm band the boards leave under the old roof, so this
# number is what raises the box: 60 -> 71 mm.
CHARGER_H = 20.0
CHARGER_AT = (-9.0, 2.0)         # tray-relative centre of the board

TRAY_T = 2.5                     # the tray plate itself
POST_TOP = 46.0                  # tray underside: 1 mm over the ESP's 45
TRAY_TOP = POST_TOP + TRAY_T

# --- envelope -------------------------------------------------------------
# The height is the only dimension that is not fixed by the parts inside: it is
# whatever the charger on its tray needs, and never less than the 60 mm this box
# shipped with, so a short charger changes nothing.
ENV_X, ENV_Y = 88.0, 78.0

WALL = 2.0        # side and roof wall
ENV_Z = max(60.0, math.ceil(TRAY_TOP + CHARGER_BOSS_H + CHARGER_H + 0.5 + WALL))
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
# Given relative to the ceiling, not as absolutes: the outlets belong high in
# the wall, and the box grew a roof-ward. At ENV_Z = 60 these are the 38/44/50
# this box has always had.
VENT_Z = [Z_CEIL - 20.0, Z_CEIL - 14.0, Z_CEIL - 8.0]
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

# Columns carrying the charger tray. Three, not four, and the missing one is
# the +X/-Y corner: the ESP's 32 x 41 footprint reaches x = 17 at y = -35..6,
# and the only gap left there is the 2 mm between it and the cell lane. Three
# points carry a 20 g board perfectly well and are statically determinate into
# the bargain; a fourth would have meant moving the ESP, which is the one
# footprint that cannot avoid the centre.
#
# Each one sits in a pocket the existing parts leave: -34/-8 between the vent
# chamber and the ESP, 15.5/30 between the HX711 and the cell, -18.5/-27
# between the chamber and the ESP's -X edge. They rise from the floor plate
# because that is the part that prints anchor-down -- a column standing free in
# the body would start in mid-air, the body printing roof-down.
# Each carries its own diameter, because the pockets are not the same size and
# a single number would have to be the smallest of them. The -X one has 25 mm
# of clear floor and gets a foot; the other two sit in gaps exactly 7 mm wide --
# between the HX711 and the cell lane, and between the vent chamber and the ESP
# -- so they are 5 mm with a millimetre either side, and no foot, because a foot
# is something a board has to be threaded past during assembly.
POSTS = [(-34.0, -8.0, 9.0), (15.4, 30.0, 6.6), (-18.5, -27.0, 6.8)]
POST_FOOT_MIN = 8.0                      # below this, no flare at the root
# Bore for an M3 heat-set insert, as the body's corner posts already use. Each
# column is as fat as its pocket allows and no fatter, which is what sets the
# wall left around the bore:
#
#   ( -34, -8)  9.0 mm -> 2.75 mm of wall   the one pocket with room to spare
#   (15.4, 30)  6.6 mm -> 1.55 mm           HX711 ends at x 12, cell guide at 18.8
#   (-18.5,-27) 6.8 mm -> 1.65 mm           vent chamber to -22, ESP from -15
#
# The last two are a tenth back from their pockets' edges rather than filling
# them: the cell lane has 0.4 mm of slack in it for a 12 mm cell, and a column
# bulging into that slack is a cell that does not drop in.
#
# A search over the plan says 8.5 mm fits in exactly one place on this floor,
# and it is the first of the three. The other two are thinner than the 2.5 mm
# the body's posts leave, which that part of this file already calls thinner
# than one would choose. It is accepted here because of what hangs on them: a
# 20 g board, held by three screws, where the insert exists so the screw can be
# undone more than once rather than because the joint is working hard. Warm
# those two inserts gently and do not lean on them.
POST_BORE, POST_BORE_H = 3.5, 6.0

# --- bending-beam anchor (M4) ---------------------------------------------
# Interface to the bending-beam clamp. The clamp's own model lived in this
# file until it was retired -- `git show d0df819:models.py` still has it. The
# mating dimensions stay here, because the anchor is meaningless without them.
ANCHOR_PITCH = 15.0              # clamp screw pitch
ANCHOR_HOLE = 4.3
NUT_AF, NUT_T = 7.2, 3.4         # M4 nut across flats, thickness
ANCHOR_CSK = 8.6                 # M4 countersunk head, plus a little
PAD_X, PAD_Y, PAD_H = 27.0, 14.0, 3.5
FLANGE_X, FLANGE_Y, FLANGE_H = 33.0, 18.0, 2.5
NUT_BOSS_H = 5.0                 # boss inside the box carrying the nuts

# Boards sit on rails this high, which is exactly the nut boss. The anchor
# then lives *under* the ESP instead of fighting it for floor area -- and the
# ESP's 42 mm depth is the one footprint that cannot avoid the centre.
DECK_Z = FLOOR_T + NUT_BOSS_H    # 8

# Panel lead, in through the FLOOR. Not the roof, which would give away the one
# property this shape exists for -- and not a side wall either, which is where
# this started and was wrong for a reason that has nothing to do with water:
# the gland would have held the cable captive on the *body* while the charger it
# feeds stands on the *floor plate*, so every opening of the box would pull the
# two apart against a tethered cable. Through the floor, cable, charger and
# anchor all stay with the same part and the body lifts off clean.
#
# The position is not a choice. A search over the plan for somewhere a 13 mm
# boss clears the boards, their ribs, the cell lane, the vent chamber, the
# anchor pad, the load-cell cable, the drain, the corner posts and the tray
# columns leaves exactly one pocket, 2 mm across, at (-24, 1).
GLAND_HOLE = 8.2                 # M8 gland; the 3 mm floor suits its panel range
GLAND_XY = (-24.0, 1.0)
GLAND_SEAT = 13.0                # flat the nut needs inside, kept clear of ribs

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

# Boss inside the box, so the anchor screws run through 14 mm of material. The
# pad and flange that used to hang below this plate are now their own part --
# see `terrasse_anchor` -- which is what leaves this underside flat.
floor = floor.union(_box(PAD_X, PAD_Y, NUT_BOSS_H, (0, 0, FLOOR_T)))

anchor_pts = [(-ANCHOR_PITCH / 2, 0.0), (ANCHOR_PITCH / 2, 0.0)]
anchor_z0 = 0.0
# The screws run downward now -- head inside the box, nut under the spacer,
# which is the end that is in the open air when this is assembled. It used to be
# the other way, with captive nuts in pockets here: a nut that has to be held in
# a pocket while a screw is turned from below, in a box whose floor also carries
# the boards, and which is lost inside a sealed enclosure if it drops.
floor = (
    floor.faces(">Z").workplane()
    .pushPoints(anchor_pts)
    .cskHole(ANCHOR_HOLE, ANCHOR_CSK, 90)
)

# Load-cell cable. The collar that used to hang below this hole and shed water
# off the lead is gone with everything else on this face: it was belt and
# braces over the drip loop, which is what actually keeps water out of a
# downward hole, and which the panel lead needs anyway. A chamfer takes its
# place, so the lead is not dragged over a printed edge.
floor = floor.cut(
    cq.Workplane("XY").circle(CABLE_D / 2).extrude(FLOOR_T)
    .translate((CABLE_XY[0], CABLE_XY[1], 0))
)
floor = floor.faces("<Z").edges(cq.NearestToPointSelector((CABLE_XY[0], CABLE_XY[1], 0))).chamfer(1.0)

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

# Panel lead. A plain bore: the gland is fitted from below -- the weather side,
# where its flange and gasket belong -- and its nut lands on the inside face,
# which is why the search above insisted on 13 mm of clear floor rather than
# just the hole.
floor = floor.cut(_cyl(GLAND_HOLE, FLOOR_T, (GLAND_XY[0], GLAND_XY[1], 0)))

# Columns for the charger tray, each with a fillet at the root: 43 mm of 7 mm
# column is slender, and the joint to the plate is where it would snap while
# somebody is threading a screw into the top.
for (px, py, pd) in POSTS:
    floor = floor.union(_cyl(pd, POST_TOP - FLOOR_T, (px, py, FLOOR_T)))
    if pd >= POST_FOOT_MIN:
        floor = floor.union(_cyl(pd + 4.0, 2.0, (px, py, FLOOR_T)))
    floor = floor.cut(
        _cyl(POST_BORE, POST_BORE_H, (px, py, POST_TOP - POST_BORE_H))
    )

display(floor)
_export(floor, "terrasse_floor")

# ---------------------------------------------------------------------------
# Beam anchor
# ---------------------------------------------------------------------------
# The pad the bending-beam clamp bolts to, and the flange that spreads its load
# into the floor plate. Both were part of that plate until 2026-09-18, and they
# are separate now for one reason: they were the only things on its underside,
# and while they were there the plate could not be printed without support. Laid
# anchor-down, an 84 x 74 plate steps out over a 33 x 18 flange -- a 25 mm
# horizontal overhang the whole way round.
#
# The load path does not change. The same two M4 screws run from under this pad,
# through the plate, into the captive nuts in the boss inside the box; this part
# is clamped between the screw heads and the plate rather than being the plate.
# If anything the grain is better -- a flat pad printed flat has its layers
# across the load, where a pad printed as a step on a plate had them along it.
#
# Print it pad-down. Every face is then flat or vertical.
anchor = _box(PAD_X, PAD_Y, PAD_H, (0, 0, 0))
anchor = anchor.union(_box(FLANGE_X, FLANGE_Y, FLANGE_H, (0, 0, PAD_H)))
for (px, py) in anchor_pts:
    anchor = anchor.cut(
        cq.Workplane("XY").circle(ANCHOR_HOLE / 2).extrude(PAD_H + FLANGE_H)
        .translate((px, py, 0))
    )
anchor = anchor.edges("|Z").fillet(2.0)

display(anchor)
_export(anchor, "terrasse_anchor")

# ---------------------------------------------------------------------------
# Charger tray
# ---------------------------------------------------------------------------
# A shelf over the two boards, carrying the solar charger in the 13 mm the
# ESP's 37 mm leaves under the roof. Separate part for the same reason the
# floor is one: it has to be fitted after the boards are in, and it could not
# be printed attached to either of the others.
#
# The charger is screwed down through its own four mounting holes -- 48 x 32,
# Ø3.2, read off the manufacturer's PCB file -- into M3 inserts in four sockets.
# It was held by cable ties while that pattern was unknown.
TRAY_X0, TRAY_X1 = -37.5, 17.5   # 1.5 mm clear of the cell lane at x = 19
TRAY_Y0, TRAY_Y1 = -31.0, 33.5

tray = _box(TRAY_X1 - TRAY_X0, TRAY_Y1 - TRAY_Y0, TRAY_T,
            ((TRAY_X0 + TRAY_X1) / 2, (TRAY_Y0 + TRAY_Y1) / 2, POST_TOP))

# Keep the chimney open. The vent chamber runs floor slots at the -X/-Y corner
# up to outlets high in both walls, and a shelf laid over its mouth would turn
# a draught into a pocket.
tray = tray.cut(
    _box(CH_X0 + CH_X - TRAY_X0, CH_Y0 + CH_Y - TRAY_Y0, TRAY_T,
         ((TRAY_X0 + CH_X0 + CH_X) / 2, (TRAY_Y0 + CH_Y0 + CH_Y) / 2, POST_TOP))
)

# Screw holes over the three columns, and a seat so the head does not stand
# proud into the charger.
for (px, py, _pd) in POSTS:
    tray = tray.cut(_cyl(3.4, TRAY_T, (px, py, POST_TOP)))
    tray = tray.cut(_cyl(6.2, 1.2, (px, py, POST_TOP + TRAY_T - 1.2)))

# Pass-through for the panel lead, which now comes up from the floor and would
# otherwise meet the underside of this plate. Placed off the charger's own
# footprint, on the -X side, so the cable arrives beside the board rather than
# under it.
tray = tray.cut(_cyl(10.0, TRAY_T, (-33.0, 1.0, POST_TOP)))

# The charger's own mounting holes. Cable ties stood here until the board was
# in hand and its pattern could be read off the manufacturer's PCB file rather
# than guessed; screws through a known pattern beat ties over an unknown one.
for sx in (-1, 1):
    for sy in (-1, 1):
        at = (CHARGER_AT[0] + sx * CHARGER_HOLES[0] / 2,
              CHARGER_AT[1] + sy * CHARGER_HOLES[1] / 2)
        tray = tray.union(_cyl(CHARGER_BOSS_D, CHARGER_BOSS_H,
                               (at[0], at[1], POST_TOP + TRAY_T)))
        tray = tray.cut(_cyl(
            CHARGER_BORE, CHARGER_BORE_H,
            (at[0], at[1], POST_TOP + TRAY_T + CHARGER_BOSS_H - CHARGER_BORE_H)))

display(tray)
_export(tray, "terrasse_charger_tray")

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

# The anchor screws pass all the way through and take their nuts here, in a
# hex pocket in this underside. The screws used to run the other way, with their
# heads in this counterbore and captive nuts up inside the box; the nuts moved
# down here because this face is the one you can reach with a spanner while the
# box is assembled, and because a nut dropped in there is a nut inside a sealed
# enclosure.
for sx in (-1, 1):
    px = sx * ANCHOR_PITCH / 2
    spacer = spacer.cut(_cyl(ANCHOR_HOLE, SPACER_H, (px, 0, SPACER_Z)))
    spacer = spacer.cut(
        cq.Workplane("XY").polygon(6, NUT_AF / math.cos(math.radians(30)))
        .extrude(NUT_T + 0.4)
        .translate((px, 0, SPACER_Z))
    )

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
##   wohnzimmer_tray   floor, walls and the two compartments
##   wohnzimmer_lid    flat cover, four screws, vented over both
##
## This is the second box for this node and it is not a revision of the first,
## which is in git (`git show cb9a0f9:models.py`). What changed is the node: it
## grew an SGP41, so the board compartment now holds three boards on jumper
## wires instead of one, and the interior went from 26 mm to 40 mm to take them
## standing rather than lying.
##
## That extra height is also what removed a compartment. The first box gave the
## SHT31 a vented chamber of its own, 90 mm of still air from the board, because
## a board-warmed SHT31 reports a humidity that is too low and the error lands in
## the corrected PM figures. With 40 mm to play with, the SHT31 goes *up* instead
## of *away*: it hangs at the far end of the particulate compartment, in the
## draught between its own wall vents and the lid, and the wall it stood behind
## is gone. A printed card slot held it there until it turned out to be
## unprintable; see the comment where it used to be cut.
##
## So, two compartments in a row:
##
##   -X  particulate + humidity   the SDS011 in a square pocket, the SHT31 on
##                                its wires in the 10 mm strip at the end, clear
##                                of the module and of the exhaust
##   +X  board bay                XIAO, SCD41 and SGP41, 50 mm wide, open floor:
##                                three boards on wires do not want a pocket
##                                shaped for one
##
## Both are vented on their long walls and through the lid. The board bay needs
## it twice over -- the SCD41 measures room air and the SGP41 wants the same air
## the room has, and neither can do that inside a sealed box.
##
## Print both parts flat on the plate, wz_tray floor down. Slots are vertical
## cuts in vertical walls, so only their tops bridge -- no support needed.

WZ_X, WZ_Y, WZ_Z = 141.0, 89.0, 46.0
WZ_WALL = 2.5
WZ_FLOOR = 3.0
WZ_LID = 3.0

WZ_IN_X = WZ_X - 2 * WZ_WALL      # 136
WZ_IN_Y = WZ_Y - 2 * WZ_WALL      # 84
WZ_IN_H = WZ_Z - WZ_FLOOR - WZ_LID  # 40, as asked
WZ_TOP = WZ_FLOOR + WZ_IN_H       # 43, where the wz_lid lands

WZ_BAF = 2.0

# Component envelopes. Measured values go here; everything else follows.
SDS_XY = 73.5                     # 71 x 70 module, pocket kept SQUARE so it
                                  # can be turned to any of four orientations
                                  # -- which edge carries the intake nozzle
                                  # differs between units, and the tube has to
                                  # reach the intake port.
SDS_SERVICE = 10.0                # strip along +Y for tube and wiring
SHT_STRIP = 10.0                  # strip at the -X end the SHT31 hangs in
BOARD_BAY = 50.0                  # asked for; holds all three boards

# Compartment boundaries in X, left to right.
WZ_X0 = -WZ_IN_X / 2              # -68
SHT_X1 = WZ_X0 + SHT_STRIP        # -58, where the module may start
SDS_X0 = SHT_X1                   # -58
SDS_X1 = SDS_X0 + SDS_XY          # 15.5
BOARD_X0 = SDS_X1 + WZ_BAF        # 17.5
# 17.5 -> 68 is 50.5 mm of bay, the 50 asked for plus the half millimetre that
# falls out of the pocket being square.

WZ_Y1 = WZ_IN_Y / 2               # 42
SDS_RIB_Y = WZ_Y1 - SDS_SERVICE - WZ_BAF   # 30, module stops here

WZ_POST = 8.0
# 3.5 mm for a heat-set insert, as asked. That is the M2.5 size: with an insert
# of 4.0 mm outer diameter the brass has plastic to displace, with one of 3.5 it
# has none -- check the insert in hand before melting four of them in.
WZ_PILOT = 3.5
# M2.5 countersunk: 2.9 clearance, head 4.7 plus a lip.
WZ_CLEAR, WZ_CSK = 2.9, 5.7
WZ_POST_XY = [(sx * (WZ_IN_X / 2 - WZ_POST / 2), sy * (WZ_IN_Y / 2 - WZ_POST / 2))
              for sx in (-1, 1) for sy in (-1, 1)]
# The posts at the -X end stand *inside* the SHT31 strip, which is why the
# module starts 10 mm in rather than against the wall: a 73.5 mm pocket pushed
# to the wall would have run straight into them.

# Height of the middle of the USB-C window, above the interior floor.
#
# Asked for at 20 mm, which is a statement about where the XIAO sits: a plug is
# rigid, so the window has to line up with the connector rather than the cable
# bending to meet it. On a board lying on the floor that is about 5 mm, so if
# the boards end up flat rather than stacked, this is the line to change.
USB_Z = 20.0

INTAKE_ID = 4.0                   # bore through the wall; see the intake port
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

# The one baffle left, notched at floor level so the SHT31's four wires can
# cross into the board bay.
wz_tray = wz_tray.union(_box(WZ_BAF, WZ_IN_Y, WZ_IN_H,
                       (SDS_X1 + WZ_BAF / 2, 0, WZ_FLOOR)))
wz_tray = wz_tray.cut(
    _box(3 * WZ_BAF, 8.0, WZ_IN_H, (SDS_X1 + WZ_BAF / 2, 0, WZ_FLOOR))
)

# Intake port in the +Y wall: the SDS011's own nozzle is tubed to it so the
# module draws room air rather than its own exhaust.
#
# A bare 4 mm bore through the wall, and it used to be a stub: a 6 mm cylinder
# reaching 8 mm inward from the wall's inner face into the service strip, bored
# 4 mm, there so the module's own nozzle could be tubed to it -- inboard, where
# the tube is, rather than outside the box.
#
# It came off for the same reason the SHT31's card slot did. This part prints
# floor down, so a horizontal cylinder hanging off a vertical wall at z = 12 is
# 8 mm of circular overhang begun in mid-air, and support inside a 4 mm bore is
# not support, it is a plug to be dug out afterwards.
#
# What stays is the airway, 4 mm, exactly what the stub carried: the module
# still draws room air rather than its own exhaust. What goes is somewhere for
# the tube to grip. Push the tube at the bore and seal it, or print the stub as
# its own part, on its axis where it needs no support, and glue it to the inner
# face -- the numbers to reproduce it are in this comment.
INTAKE_Z = WZ_FLOOR + 9.0
wz_tray = wz_tray.cut(
    cq.Workplane("XZ").circle(INTAKE_ID / 2).extrude(3 * WZ_WALL)
    .translate(((SDS_X0 + SDS_X1) / 2, WZ_Y1 + 2 * WZ_WALL, INTAKE_Z))
)

# Exhaust, low in the -Y wall and on the far side of the module from the intake.
for z in (6.0, 11.0, 16.0):
    wz_tray = _slots(wz_tray, 4, 17.0, (14.0, 3 * WZ_WALL, SLOT_W),
                     ((SDS_X0 + SDS_X1) / 2, -WZ_Y1, WZ_FLOOR + z), axis="x")

# Rib the module rests against, so it cannot slide into the service strip.
wz_tray = wz_tray.union(_box(SDS_XY, WZ_BAF, 6.0,
                       ((SDS_X0 + SDS_X1) / 2, SDS_RIB_Y + WZ_BAF / 2, WZ_FLOOR)))

# --- the SHT31, in the strip at the -X end ---------------------------------
# There is no card slot here any more, and this is the second time this box has
# given up a feature for the SHT31 rather than the other way round: it lost the
# sensor's own compartment to the 40 mm height limit, and now it loses the
# holder that replaced it. Removed because it could not be printed.
#
# The holder was a 6 x 18 x 34 tower with a 2 mm slot up the middle, so 2 mm of
# wall on each side of the card. What was not noticed is that the -X wall vents
# below are cut with a 3 * WZ_WALL block -- 7.5 mm of x, reaching to x = -64.25
# -- while the tower's -X wall stood at x = -66 .. -64. The vent cut therefore
# took 1.75 of that wall's 2 mm at each of three heights, leaving 0.25 mm of
# plastic holding it and the plate above the top notch standing on nothing at
# all. A slicer prints that as three courses of air.
#
# It is fixable -- narrow the vent cut, or move the tower inboard of it -- and
# it is deliberately not being fixed. The holder was never doing much: the
# comment that stood here said as much, "what matters is that the sensor sits
# above the module in moving air, not that the card is held firmly". The card
# is on four jumper wires with 90 mm of run and the strip is 10 mm wide, so the
# wires and the strip already hold it in x and y. What goes with the tower is
# the height: nothing now stops the card from resting on the floor of the strip
# instead of standing in the draught. Dress the wires so it sits high, or bring
# the holder back on the numbers above.
SHT_STRIP_X = WZ_X0 + SHT_STRIP / 2      # -63, centred in the strip

# The SHT31's air: the -X wall, high, where the card's sensor should sit.
for z in (20.0, 26.0, 32.0):
    wz_tray = wz_tray.cut(_box(3 * WZ_WALL, 22.0, SLOT_W, (WZ_X0, 0, WZ_FLOOR + z)))

# --- board bay: CO2 and VOC both need room air -----------------------------
BOARD_MID = (BOARD_X0 + WZ_IN_X / 2) / 2
for z in (8.0, 16.0, 24.0, 32.0):
    for sy in (-1, 1):
        wz_tray = _slots(wz_tray, 3, 14.0, (10.0, 3 * WZ_WALL, SLOT_W),
                         (BOARD_MID, sy * WZ_Y1, WZ_FLOOR + z), axis="x")

# Cable out, sized for a USB-C plug's overmould. Height per USB_Z above.
USB_W, USB_H = 15.0, 9.0
wz_tray = wz_tray.cut(_box(3 * WZ_WALL, USB_W, USB_H,
                     (WZ_IN_X / 2, 0, WZ_FLOOR + USB_Z - USB_H / 2)))
# ... and air around it, since this wall is otherwise solid.
for z in (8.0, 32.0):
    wz_tray = _slots(wz_tray, 2, 26.0, (3 * WZ_WALL, 16.0, SLOT_W),
                     (WZ_IN_X / 2, 0, WZ_FLOOR + z), axis="y")

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
# Edge break first: once the vent slots are cut, `>Z` edges are no longer just
# the outline, and a fillet on the 4 mm web between two slots is not a fillet.
wz_lid = _box(WZ_X, WZ_Y, WZ_LID).edges("|Z").fillet(CORNER_R)
wz_lid = wz_lid.faces(">Z").edges().fillet(TOP_BREAK)

# Over the board bay: this is the chimney's outlet, and the reason the CO2 and
# VOC readings are about the room rather than about the box. Slots run along X
# so the material between them is continuous across the lid's long span, which
# is what a cover with screws only in its corners needs.
wz_lid = _slots(wz_lid, 7, 8.0, (44.0, 3.0, WZ_LID + 2),
                (BOARD_MID, 0, -1.0), axis="y")

# Over the SHT31's end of the other compartment, for the same reason in
# miniature: the card hangs in the draught between these and its wall slots.
wz_lid = _slots(wz_lid, 3, 8.0, (16.0, 3.0, WZ_LID + 2),
                (SHT_STRIP_X + 2.0, 0, -1.0), axis="y")

# Over the SDS011 itself, which is the stretch of this lid that had nothing.
# Same 7 rows and 8 mm pitch as the board bay, so the whole cover reads as one
# grille and the webs stay 5 mm.
#
# The module's intake is not affected by this and could not be: it is tubed to
# the port in the +Y wall, so it draws what the port gives it whatever the lid
# does. What changes is the exhaust, which until now had one way out -- the
# slots low in the -Y wall, deliberately on the far side of the module -- and
# now has a second, straight up and away from the intake. That is the right
# direction for it.
#
# It also covers the case the intake stub's removal opened up. If the tube is
# not sealed at the bore, the module draws compartment air instead of room air;
# with the lid open above it, compartment air *is* room air, which is the
# failure mode one would have chosen.
#
# x runs -49 .. 13: clear of the SHT31 rows that end at -53 by a 4 mm web,
# stopping short of the baffle at 15.5 and leaving 7.75 mm to the board bay's
# grille. Nothing here goes near the corner posts or their countersinks.
wz_lid = _slots(wz_lid, 7, 8.0, (62.0, 3.0, WZ_LID + 2),
                (-18.0, 0, -1.0), axis="y")

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
## So the board bay gets its own chimney -- inlet low, outlets high in both long
## walls and a grille in the lid above the board -- and the notch stops being
## the path of least resistance. The notch itself is left alone deliberately;
## see the comment on it below.
##
## The lid is vented over both chambers, which it was not at first. Over the
## board bay it is the top of that chimney; over the sensor chamber it is not
## about heat at all but about response time, since every other opening into
## that end is a side slot the room air has to turn to come through. Both cuts
## rest on where these two boxes actually sit -- on a shelf, not under a hob or
## a shower head -- because an open lid in a kitchen or a bathroom is otherwise
## exactly where grease and condensate get in.

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
# Its own clearance and countersink, rather than the living room's. These used
# to read `WZ_CLEAR, WZ_CSK`, which was true and load-bearing at the same time:
# the two boxes happened to use the same screw, so borrowing looked tidy -- and
# when the living room moved to M2.5, this lid quietly followed it and stopped
# fitting the M3 screws the printed trays are tapped for. A number two boxes
# share has to be shared on purpose or not at all.
KL_CLEAR, KL_CSK = 3.4, 6.6   # M3 countersunk, matching the M3 insert above
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
# The kl_lid carries the top of this chimney, over the same stretch of x; see
# the grille cut into it below. This used to be walls only, on the argument
# that nothing in here needs room air the way the schlafzimmer box's SCD41
# does -- true of the board, and it answers the wrong question about it. The
# board does not need room air, it needs its heat gone, and warm air leaves
# upward. A high wall slot is where the warm air has to turn to reach it; a lid
# slot is where it was already going.
#
# Bounded on BOTH sides, and the -X bound is the one worth explaining. The four
# jumpers to the SHT31 leave the board at its -X end, loop in the 8.5 mm of
# slack between board and baffle, and drop through the notch. That corridor runs
# x = -6 .. 2.5, and the first version of these slots started at x = 3.0, hard
# against the board edge with its lowest row at 8..10.5 mm -- which is exactly
# where a jumper housing sits. A wire passes a 2.5 mm slot easily, so that was
# an invitation to snag one while closing the box.
#
# Starting at x = 6 instead leaves the whole corridor plus 3.5 mm of margin with
# no opening in either wall, at any height. The +X bound keeps the slots inboard
# of the corner posts (x >= 22.5), so each opens onto air rather than onto a
# post. The heat source does not mind: the ESP32 sits mid-board, still under the
# slots.
KL_BOARD_VENT_X = 13.5
KL_BOARD_VENT_L = 15.0
for z in (5.0, 12.0, 19.0):
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

# ---------------------------------------------------------------------------
# Lid
# ---------------------------------------------------------------------------
# Edge break before the slots, for the reason spelled out on the schlafzimmer
# lid: once they are cut, `>Z` is no longer just the outline, and TOP_BREAK on
# a 3 mm web is a failed kernel call rather than a fillet.
kl_lid = _box(KL_X, KL_Y, KL_LID).edges("|Z").fillet(CORNER_R)
kl_lid = kl_lid.faces(">Z").edges().fillet(TOP_BREAK)

# Grille over the board bay -- the top of the chimney whose inlet and wall
# outlets are cut into the kl_tray above. 5 x 15 x 2.5 = 187 mm2, against the
# 225 in the walls, and it is the opening the stack effect actually uses: the
# wall outlets sit at z = 19 of a 24 mm bay, so warm air has to turn to reach
# them.
#
# The x bounds are the wall slots' bounds, deliberately shared: KL_BOARD_VENT_X
# and _L already encode both the jumper corridor that must stay blank (x = -6
# .. 2.5, plus margin) and the corner posts the openings must stay inboard of.
# Rows run along x and step across y, so each one spans the board rather than
# the gap beside it, and the 3 mm webs between them run the short way.
kl_lid = _slots(kl_lid, 5, 5.5, (KL_BOARD_VENT_L, SLOT_W, KL_LID + 2),
                (KL_BOARD_VENT_X, 0, -1.0), axis="y")

# Grille over the sensor chamber. 4 x 12.5 x 2.5 = 125 mm2, on top of the 150
# in the -X end wall and the 120 in the two long walls, and it is not there for
# heat -- this end has no heat in it. It is there for response time. Every
# other opening into this chamber is a side slot, so room air reaches the SHT31
# by crossing a wall and turning; a lid slot puts it straight down onto the
# card, which stands at x = -19 directly under the grille.
#
# What this costs, and why it is affordable here. An opening over the sensor is
# the one thing this lid was originally solid to avoid: these boxes go to a
# kitchen and a bathroom, and grease or a drop of condensate lands on the one
# part that never reports its own failure -- a wet RH sensor still returns a
# number. The reason it is cut anyway is placement, not modelling: both nodes
# sit on a shelf, not under a hob and not under a shower head. That is a fact
# about where the box lives rather than about the box, so it is the first thing
# to re-check before this design is printed for a third room.
#
# Bounds. x = -21.5 .. -9 -- short of the baffle at x = -8, which stands full
# height and would otherwise take a slot's far end, and inboard of the -X
# corner posts at x <= -22.5, so each row opens onto air. Four rows, not the
# board bay's five: the outer pair of five would run to y = +-12.25, and with
# the row ends already 5 mm from the countersinks at (-26.5, +-13.75) that
# leaves 1.9 mm of lid in the corner. Four rows keep the ends at y = +-9.5 and
# the thinnest section at 3.3 mm, which is more than the 2.4 mm the board
# bay's grille leaves and more than the 2.45 mm this lid already has at its
# edge.
KL_SENS_VENT_X = -15.25
KL_SENS_VENT_L = 12.5
kl_lid = _slots(kl_lid, 4, 5.5, (KL_SENS_VENT_L, SLOT_W, KL_LID + 2),
                (KL_SENS_VENT_X, 0, -1.0), axis="y")

kl_lid = (
    kl_lid.faces(">Z").workplane()
    .pushPoints(KL_POST_XY)
    .cskHole(KL_CLEAR, KL_CSK, 90)
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


## ===========================================================================
## Wasserzähler — camera tube for the two water meters
## ===========================================================================
##
## Four printed parts, one design, printed twice -- once for the cold meter and
## once for the hot one, the way climate_tray serves both kueche and bad:
##
##   wasserzaehler_tube    saddle that cradles the meter, and the barrel
##   wasserzaehler_strap   the other half of the clamp
##   wasserzaehler_ring    drop-in carrier for the illumination LEDs
##   wasserzaehler_cap     lens plate, camera pocket, and the board bay
##
## Two things about this section are exceptions, and both are deliberate.
##
## The name points at no node. Every other part in this file is named for a
## `NODE=` slug in node.rs; `wasserzaehler` is not one and never will be, since
## these meters are read by an ESP32-CAM running jomjol's AI-on-the-edge-device
## and not by this crate at all. It is here because this is where the CAD
## toolchain is, and a second copy of that toolchain would cost more than one
## stretched convention. The meters themselves are documented in
## `nixos-private/docs/zaehler.md`.
##
## And this is the first part in this file that is not a box with a board in
## it. It is an optical fixture, and every dimension below follows from four
## measurements and one lens.
##
## What is being looked at. Two ZENNER Minomess A B.One, cold and hot, in the
## riser shaft behind the bathroom mirror cabinet. They are capsule meters: a
## white cylinder 64 mm across, held in a wall fitting by a union ring that
## carries the calibration seal, dial facing straight out at the mirror. That
## cylinder is the whole reason this part is simple -- a long, smooth,
## concentric-by-construction clamping surface, so the tube goes on like a lens
## hood and needs no alignment feature at all.
##
## And the pipe runs VERTICALLY (confirmed at the meter, 2026-09-19). The
## capsule therefore stands upright and its dial looks horizontally out of the
## shaft, which puts the axis of this tube horizontal as well. Nothing in the
## assembly is held down by gravity: the board hangs off the back of a lying
## tube, the LED ring stands on edge in its seat. Both need positive retention
## and neither gets it from being put down carefully. Anything below that reads
## as "it drops in" means "it is captured", never "it rests there".
##
## It also settles what "up" means, which is not a question this file can
## answer. Up in the picture is a rotation about the tube axis, decided by
## where the clamp lands on the capsule -- not by +Y, not by any axis here. A
## seat offset meant to centre the window has to be derived from a photograph
## taken in the mounted position, and until there is one, the seat belongs on
## the axis.
##
##   capsule outer diameter       64.0   caliper
##   mirror frame to dial face    90.0   folding rule
##   reading window (bezel)       37 x 23
##   digit row                    32 x 4.7
##
## The last two are not caliper figures. They are ratios read off a square-on
## photograph and scaled by the 64 mm. Honest enough for framing -- nothing
## here touches the window -- but they do not get to become tolerances, and
## nothing below depends on them except the check that the LED ring does not
## shade the window.
##
## The optics, which set the length. This is a measured focus distance and no
## longer a computed framing. The draft that dimensioned this part at 62 mm
## reasoned from the stock lens -- 55 deg horizontal, frame 1.04 x distance,
## 64.4 mm at 62 mm, near enough to swallow the capsule whole. That lens is
## gone: on 2026-09-18 the OV2640 on board 1 was screwed out of its factory
## setting, which was fixed near 300 mm and left the dial unreadable from
## anywhere inside this tube. See `nixos-private/docs/zaehler.md`.
##
## Extending a lens narrows its field, so 1.04 does not survive the change and
## the frame below is read off the acceptance photograph instead of derived.
## The 37 mm reading window spans roughly 70 % of frame width there, putting
## the frame near 53 mm and the digit row at ~60 px per digit at VGA -- four
## times that at UXGA, against the 32 x 20 px the digit net consumes.
## Resolution was never the constraint at either length.
##
## What the shortening costs is the framing argument, and it should be stated
## plainly: at 62 mm the frame took in the whole 64 mm capsule, and jomjol's
## alignment could lock onto that outline. It cannot now. It aligns on the
## bezel and the printed text inside the window instead -- which is what it
## matches on anyway, but with 53 mm of frame around a 37 mm window there is
## only ~8 mm of lateral slack before the window clips. The clamp has to sit
## square; on a concentric capsule it does, which is the one thing that makes
## this acceptable.
##
## The barrel bore is 62 and not 65 because the step at z = 0 doubles as the
## axial stop -- it sits on the meter's face and sets that 40 mm, instead of
## leaving it to wherever the clamp happened to be tightened.
##
## Depth has stopped being the tight dimension:
##
##   dial face to lens front      40
##   lens plate                    3
##   board bay                    20
##   ----------------------------------
##   forward of the dial face     63   of 90 available
##
## 27 mm of slack where the 62 mm draft had 5. The mirror door is no longer a
## question worth checking.
##
## Still unmeasured: the working distance itself. 40 mm is where a hand-held
## board looked sharp, not a caliper reading, and the photograph hints the true
## peak sits further out than that. Before the second barrel is printed, find
## the sharpness peak again with the folding rule in frame and read it off.
##
## Why the clamp is two parts. The first draft had a slit sleeve with an ear
## and one screw, like a shaft collar, and it would not have worked: the slit
## ran 20 mm and then the barrel closed over it, so pulling the ear together
## would have had to ovalise 60 mm of stiff tube. A collar can be a spring only
## if it is a spring all the way along. Splitting it properly costs one part
## and removes the question -- and it costs nothing in light, because the whole
## clamp sits *behind* the dial face where there is no light path to spoil.
##
## The two halves stop 0.4 mm short of the split plane each, so the screws have
## somewhere to travel; they close onto 64.0 and not onto each other.
##
## Why the bay has no back. Two requirements before they were conveniences.
## `zaehler.md` needs the microSD reachable, because the firmware serves its
## web UI and its whole configuration off the card. And the shaft is a
## bathroom: a sealed tube against a cold-water capsule is a condensation trap,
## and a fogged lens is a reading lost every time somebody showers. An open
## back is the vent and the card slot at once -- which is why there are no vent
## slots in the barrel, since those would only be a second way for light in
## when the mirror is open.
##
## Lighting, and why it is not the flash LED. The board's own LED sits ~15 mm
## off the lens axis. Its reflection in the dial glass lands half that off the
## frame centre, ~7 mm, and the digit row is 32 mm wide: the highlight falls
## inside the text. So the light comes from a ring at z = 8..16 pointing *back*
## at the dial from 30 mm out -- 60 deg off axis, which throws the specular
## lobe 60 deg to the other side and nowhere near the lens. The ring is its own
## part on purpose: it is the one thing here that will want a second try, and
## reprinting a 6 mm annulus is cheaper than reprinting the barrel.
##
## Its 52 mm bore is a sight line, not a fit, and it is also why the ring sits
## at 14 and not at 8 where it started. The narrowest thing in the tube is meant
## to be the 62 mm barrel bore and nothing else; the cone from the lens is 54 mm
## across at z = 8 and a 52 mm ring clipped it -- 0.17 cm3 of ring inside the
## view, found by intersecting the two rather than by trusting the arithmetic.
## At z = 14 that cone is 48 mm and the same ring clears it by 2 mm all round.
##
## Print the tube and the cap standing on their LENS ENDS, saddle and bay
## upward, and both go down without support. On the tube that is because the
## barrel is a full annulus and the saddle only half of one: printed the other
## way up, the missing half would start 180 degrees of 3 mm ledge in mid-air.
## This way material only ever disappears going up, the three flange bosses
## included -- they grow down from the very face that meets the bed.
##
## On the cap it is because the lens plate IS that face. It was not always: see
## the note at WU_CAP_FIT for the socket this replaced and the 4038 mm2 of
## ceiling it cost. The strap and the ring print flat.
##
## PETG, not PLA. The clamp is under sustained load in a warm damp room and PLA
## creeps; the rest of this file is PLA because nothing in it is a spring.

WU_CAP_D = 64.0                  # measured, caliper, 2026-09-15
WU_CLAMP_BORE = WU_CAP_D         # the two halves close ONTO this, not past it
WU_BORE = 62.0                   # barrel, and the reason is the shoulder below
WU_OD = 71.0

WU_WORK = 40.0                   # dial face to lens front; see the optics note
WU_CLAMP_L = 24.0
WU_Z0 = -WU_CLAMP_L              # -24, back end of the saddle
WU_SPLIT = 0.4                   # each half stops this far short of y = 0

# Its own clearance and pilot rather than the climate box's, deliberately: that
# comment is three boxes up and it is the same trap. 3.5 is right here for the
# same reason it is right there -- a heat-set M3 displaces plastic rather than
# cutting it -- but it holds because it is restated, not because it is imported.
WU_CLEAR, WU_PILOT = 3.4, 3.5

# The strap's two holes get their own, wider figure. 3.4 is an M3 clearance
# that assumes the hole lands where the model put it; on the strap it has to
# line up with an insert in the other half across a printed parting line, and
# that is one tolerance stack too many. Its own constant rather than opening
# WU_CLEAR, which the cap lugs and the bar also use and which is fine there --
# those meet a boss on the same part.
WU_STRAP_CLEAR = WU_CLEAR + 0.5              # 3.9

WU_EAR_X = 40.0                  # centre; the 10 mm ear overlaps the barrel by
WU_EAR_W = 10.0                  # 0.5 mm so it unions instead of touching
WU_EAR_Y = 9.0
WU_EAR_Z0, WU_EAR_H = -22.0, 20.0

# The strap's ears are not the tube's. Two differences, both measured on the
# printed part rather than guessed.
#
# They run the FULL clamp length instead of being inset 2 mm at each end. The
# strap prints standing on its end face, so an inset ear begins 2 mm above the
# bed over thin air: 176 mm2 of ceiling, which was all the support this part
# ever asked for.
#
# And they reach INWARD past the barrel surface instead of stopping at it. A
# 10 mm box against a 71 mm cylinder touches only along a sliver near y = 0 --
# by y = 9.4 the cylinder has fallen away to x = 34.2 while the box face stands
# at 35, so the ear hung off a thread of material with a gap along the top.
# Reaching to 33 puts the box inside the solid, and the 64 mm clamp bore, cut
# afterwards, trims it back flush with the rest of the ring.
WU_EAR_IN = 33.0                             # inner edge, inside the 71 surface
WU_EAR_OUT = WU_EAR_X + WU_EAR_W / 2         # 45, outer edge unchanged
WU_EAR_TAPER = 7.0               # 45 deg on the outer face; see the ear union
WU_EAR_TIP = 3.0                 # what is left of the 10 mm at the tip
WU_SCREW_Z = -12.0

# Illumination band. The seat is bored 4 mm into a 3 mm wall, so the wall has
# to come from somewhere -- hence the band. 77 outside against a 69 bore leaves
# 4 mm, which is what the wire holes pass through.
WU_BAND_OD = 77.0
WU_BAND_Z0, WU_BAND_Z1 = 11.0, 25.0
WU_SEAT_D = 69.0
WU_SEAT_Z0, WU_SEAT_Z1 = 14.0, 22.0
WU_SEAT_LEAD = 3.5               # 45 deg lead-in; see the seat cut
WU_WIRE_D = 3.2
WU_WIRE_Z = 18.0

# Cap. It bolts to the barrel's end face through three lugs; that face is what
# sets the working distance, as it did before.
#
# It used to slip over the barrel instead, on a 12 mm socket held by one M3
# grub. That joint printed badly and the shape is why, not the orientation. A
# socket below the plate and the board bay above it put a cavity on each side
# of a plate in the middle of a tube, so whichever end went on the bed, the
# plate spanned the one underneath it as a ceiling: 4038 mm2 of it, measured
# off the mesh, against 598 mm2 of bed contact on a 2.5 mm ring. Both ends
# measured the same, within 30 mm2 -- there was nothing to gain by flipping it.
#
# Deleting the socket makes the lens plate the bottom face: 4430 mm2 flat on
# the bed and no ceiling anywhere. What is left overhead are the two slots in
# the bay wall, 4.2 and 12 mm across, which bridge.
#
# The cost is honest and small. The 12 mm overlap that used to light-seal this
# joint is now a plane face pulled together by three screws -- less of a
# labyrinth, but it sits 40 mm in front of the dial and well outside the view
# cone, so a hairline there is not a light path onto the glass.
WU_CAP_FIT = 0.4
WU_CAP_BORE = WU_OD + WU_CAP_FIT             # 71.4, now just the bay bore
WU_CAP_WALL = 2.5
WU_CAP_OD = WU_CAP_BORE + 2 * WU_CAP_WALL    # 76.4
WU_PLATE = 3.0
WU_BAY = 30.0

# The bay is 30 and not 20 because the ESP32-CAM keeps its MB carrier board.
# That board is what makes this practical: the camera board plugs into it on
# its own headers, so only the carrier has to be held, and it brings the micro
# USB socket that feeds the thing for good. Measured on the bench, both boards
# together and without the camera module: 16.8 mm.
#
#   lens plate back      43.0
#   lens holder          43.0 .. 48.5
#   module PCB           48.5 .. 52.5
#   board stack          52.5 .. 69.3   <- 16.8, measured
#   hold-down bar        69.3 .. 72.3
#
# Depth was the tight dimension when this part wanted 62 mm of barrel. It is
# not any more: 73 of the 90 mm available, and 17 to spare.
WU_MB_L, WU_MB_W = 40.0, 27.0    # carrier outline, measured
WU_STACK = 16.8                  # both boards, no camera module, measured

# Retention, v2. The cable tie it replaces never had anything to pull against:
# the slot sat 14 mm up the bay and the board ends around 54, so it crossed
# behind the board without touching it. Tape was the result.
#
# Neither board has a mounting hole -- not a clone quirk, the AI-Thinker
# outline has none at all, it is meant to sit on a header. So nothing here
# screws THROUGH the board. The camera stack locates it from the front, two
# posts and a bar press it back onto that, and the board is merely clamped.
#
# Posts on +/-X, because -Y is where the USB has to come out. R 30 is what the
# 71.4 bore allows while leaving the bar short enough to go in: 68 mm between
# a 71.4 wall.
WU_POST_R, WU_POST_D = 30.0, 8.0
WU_BAR_L, WU_BAR_W, WU_BAR_T = 68.0, 12.0, 3.0

# USB window. A notch open to the back edge, not a hole: printed bay-upward it
# then needs no bridge, and a micro USB plug is ~10 mm long against 8.7 mm of
# radial room inside the wall -- a neat cut-out would have been a slot the plug
# could not reach through. +/-30 deg is loose on purpose, since which of the
# three flange positions ends up pointing down is not something the CAD gets to
# decide. Nothing optical is at stake: this is all behind the lens plate.
WU_USB_X, WU_USB_Z0 = 18.0, 56.0
WU_LENS_D = 14.0                             # clears the cone with room to spare

# The flange. Three lugs at 120 deg, on a bolt circle just outside both bodies
# so each lug unions rather than touches. The tube's are 10 mm tall and grow
# downward from the same end face, which is the end it prints on -- so they lie
# flat on the bed too, and material only ever disappears going up.
WU_LUG_N = 3
WU_LUG_R = 39.0                              # bolt circle; barrel R is 35.5
WU_LUG_D = 10.0                              # 3.25 mm of wall around a heat-set M3
WU_LUG_H = 10.0                              # tube-side boss, takes the insert
WU_LUG_A0 = 90.0                             # first lug at +Y, clear of the clamp ears

# And a channel through the bay wall over each lug, or the screws cannot be
# fitted at all. The bolt circle is R 39 and the cap wall runs to R 38.2, so a
# 5.5 mm M3 head spans R 36.25 to 41.75 and buries 2 mm of itself in the wall;
# a driver shaft is no better. Moving the circle out instead would have taken
# the cap to 94 mm and wanted 16 mm bosses on the barrel to reach it -- wider
# than the clamp, to solve something a 9 mm slot solves.
#
# Full height, because the driver comes in along the axis and a head-sized
# pocket would still leave the shaft against the wall. Three 13 deg gaps in a
# 71.4 bore, all of it behind the lens plate: nothing optical, and the bay is
# already open at the back and notched for the USB.
WU_LUG_ACCESS = 9.0

# The camera module rides on a ribbon, so it is located by the cap and not by
# the board. Nominal, from the OV2640 module that ships with these boards --
# 8.5 mm square lens holder on a PCB about the same. NOT measured: the module
# was still in its bag when this was drawn, and this pocket is the one feature
# most likely to want a second print.
WU_MOD_SQ = 8.8
WU_MOD_L = 5.5
WU_MOD_PCB = 11.0
WU_BOSS = 16.0

# Derived here and not up with the other retention numbers, because both of
# these need WU_MOD_L, which is a camera dimension and belongs with the camera.
WU_BOARD_Z = WU_WORK + WU_PLATE + WU_MOD_L + 4.0     # 52.5, front of the stack
WU_POST_H = WU_STACK + WU_MOD_L + 4.0                # plate to the back of it

# Board retention, v1: a cable tie, not a screw pattern. The ESP32-CAM clones
# do not agree on where their mounting holes are, and a tie takes any of them
# while leaving the card edge clear.

# Flat USB cable in. A slot, not a gland: the cable is ~1.5 mm thick, and a
# round gland would have wanted a bend radius the shaft has not got.


def _lugs():
    """The three flange positions, shared by the cap and the barrel.

    One helper rather than two loops with the same trigonometry in them: the
    two parts have to agree on this circle or the screws do not go in, and the
    cheapest way to guarantee that is to have only one copy of it.
    """
    return [
        (WU_LUG_R * math.cos(math.radians(WU_LUG_A0 + i * 360.0 / WU_LUG_N)),
         WU_LUG_R * math.sin(math.radians(WU_LUG_A0 + i * 360.0 / WU_LUG_N)))
        for i in range(WU_LUG_N)
    ]


def _along_y(d, length, at):
    """Cylinder lying along +Y from at[1], centred on at[0]/at[2].

    The two helpers at the top of this file both build along Z, and every box
    in this file only ever needed that. A clamp screw runs across the axis, so
    rather than guess at Workplane("XZ")'s extrusion direction, rotate a known
    thing: about +X by -90 takes +Z to +Y.
    """
    return (
        _cyl(d, length)
        .rotate((0, 0, 0), (1, 0, 0), -90)
        .translate(at)
    )


# ---------------------------------------------------------------------------
# Tube
# ---------------------------------------------------------------------------
wu_tube = _cyl(WU_OD, WU_WORK, (0, 0, 0))
wu_tube = wu_tube.union(
    _cyl(WU_BAND_OD, WU_BAND_Z1 - WU_BAND_Z0, (0, 0, WU_BAND_Z0))
)
# The band's far shoulder, at 45 deg. Printed lens-end-down the layers arrive
# here from z = 40, so a square step at z = 25 is 3 mm of annular ledge over
# air -- 697 mm2 of it, the largest overhang left on this part after the cap
# stopped needing one. The near shoulder at z = 11 needs nothing: going that
# way material only ever leaves.
wu_tube = wu_tube.union(
    cq.Workplane("XY").circle(WU_BAND_OD / 2)
    .workplane(offset=(WU_BAND_OD - WU_OD) / 2).circle(WU_OD / 2)
    .loft().translate((0, 0, WU_BAND_Z1))
)

# Saddle: the same annulus, then everything on the +Y side of the split taken
# away. The cut box is deliberately larger than the part in every direction;
# only its -Y face does any work.
wu_saddle = _cyl(WU_OD, WU_CLAMP_L, (0, 0, WU_Z0))
wu_saddle = wu_saddle.cut(
    _box(WU_OD + 4, WU_OD + 4, WU_CLAMP_L + 2,
         (0, (WU_OD + 4) / 2 - WU_SPLIT, WU_Z0 - 1))
)
# Ears, and the top 7 mm of each is a taper rather than a square end. That end
# face is the one the printer reaches first -- 90 mm2 of it per ear, hanging in
# air 42 mm up. The taper keeps the INNER face where it is, at the barrel, and
# pulls only the outer face in: 7 mm across 7 mm is 45 deg, and what is left at
# the tip still lands on the barrel instead of starting as an island. The
# screw at z = -12 stays inside the full section.
for sx in (-1, 1):
    wu_saddle = wu_saddle.union(
        _box(WU_EAR_W, WU_EAR_Y, WU_EAR_H - WU_EAR_TAPER,
             (sx * WU_EAR_X, -WU_EAR_Y / 2 - WU_SPLIT, WU_EAR_Z0))
    )
    wu_saddle = wu_saddle.union(
        cq.Workplane("XY").rect(WU_EAR_W, WU_EAR_Y)
        .workplane(offset=WU_EAR_TAPER)
        .center(-sx * (WU_EAR_W - WU_EAR_TIP) / 2, 0)
        .rect(WU_EAR_TIP, WU_EAR_Y - 2 * WU_EAR_TAPER / 2)
        .loft()
        .translate((sx * WU_EAR_X, -WU_EAR_Y / 2 - WU_SPLIT,
                    WU_EAR_Z0 + WU_EAR_H - WU_EAR_TAPER))
    )
wu_tube = wu_tube.union(wu_saddle)

# Bores last, so the saddle's ears cannot fill them back in.
# Two bores, and the 1.0 mm step between them at z = 0 is the whole point: it
# lands flat on the meter's face, around the bezel, and that is what fixes the
# working distance and squares the tube up. Without it the clamp holds the tube
# wherever it happened to be pushed, and 62 mm becomes a hope.
#
# It costs field. The barrel bore is the narrowest aperture in the tube, so the
# camera sees a 62 mm circle of a 64 mm capsule -- 1 mm of rim off each side.
# The window is 37 mm and sits well inside that, and jomjol's alignment wants
# the bezel and the printed text, both of which are inside it too.
wu_tube = wu_tube.cut(_cyl(WU_BORE, WU_WORK + 1, (0, 0, 0)))

# Flange bosses, grown down from the end face the cap bolts to.
for _x, _y in _lugs():
    wu_tube = wu_tube.union(
        _cyl(WU_LUG_D, WU_LUG_H, (_x, _y, WU_WORK - WU_LUG_H))
    )
for _x, _y in _lugs():
    wu_tube = wu_tube.cut(
        _cyl(WU_PILOT, WU_LUG_H + 1, (_x, _y, WU_WORK - WU_LUG_H - 0.5))
    )
wu_tube = wu_tube.cut(_cyl(WU_CLAMP_BORE, WU_CLAMP_L + 1, (0, 0, WU_Z0 - 1)))
# LED seat, and the lead-in is not decoration. Printed lens-end-down, the
# layers run from z = 62 towards the meter, so the seat's far face at z = 22 is
# the last solid layer over a cavity and prints itself. Its near face would have
# been 3.5 mm of annular cantilever starting in mid-air, and that is the face
# the ring seats against. A 45 deg lead-in instead: going that way the bore
# closes gradually and holds itself up.
wu_tube = wu_tube.cut(
    _cyl(WU_SEAT_D, WU_SEAT_Z1 - WU_SEAT_Z0, (0, 0, WU_SEAT_Z0))
)
wu_tube = wu_tube.cut(
    cq.Workplane("XY").circle(WU_BORE / 2)
    .workplane(offset=WU_SEAT_LEAD).circle(WU_SEAT_D / 2)
    .loft().translate((0, 0, WU_SEAT_Z0 - WU_SEAT_LEAD))
)

# Clamp screws: the insert lives in the saddle, so taking the strap off does
# not take the thread with it.
for sx in (-1, 1):
    wu_tube = wu_tube.cut(
        _along_y(WU_PILOT, WU_EAR_Y + 2,
                 (sx * WU_EAR_X, -WU_EAR_Y - WU_SPLIT - 1, WU_SCREW_Z))
    )

# LED wiring, out through the band on the side away from the ears.
for sx in (-1, 1):
    wu_tube = wu_tube.cut(
        _along_y(WU_WIRE_D, WU_BAND_OD,
                 (sx * 14.0, -WU_BAND_OD / 2 - 1, WU_WIRE_Z))
    )

display(wu_tube)
_export(wu_tube, "wasserzaehler_tube")

# ---------------------------------------------------------------------------
# Strap
# ---------------------------------------------------------------------------
wu_strap = _cyl(WU_OD, WU_CLAMP_L, (0, 0, WU_Z0))
wu_strap = wu_strap.cut(
    _box(WU_OD + 4, WU_OD + 4, WU_CLAMP_L + 2,
         (0, -(WU_OD + 4) / 2 + WU_SPLIT, WU_Z0 - 1))
)
for sx in (-1, 1):
    wu_strap = wu_strap.union(
        _box(WU_EAR_OUT - WU_EAR_IN, WU_EAR_Y, WU_CLAMP_L,
             (sx * (WU_EAR_OUT + WU_EAR_IN) / 2,
              WU_EAR_Y / 2 + WU_SPLIT, WU_Z0))
    )
wu_strap = wu_strap.cut(_cyl(WU_CLAMP_BORE, WU_CLAMP_L + 2, (0, 0, WU_Z0 - 1)))
for sx in (-1, 1):
    wu_strap = wu_strap.cut(
        _along_y(WU_STRAP_CLEAR, WU_EAR_Y + 2,
                 (sx * WU_EAR_X, WU_SPLIT - 1, WU_SCREW_Z))
    )

display(wu_strap)
_export(wu_strap, "wasserzaehler_strap")

# ---------------------------------------------------------------------------
# The LED ring is gone
# ---------------------------------------------------------------------------
# It carried six 5 mm LEDs to light the dial from 60 deg off axis, on the
# assumption that the board's own flash could not do the job. It can: at
# LEDIntensity near 1 it lights the dial in a closed shaft better than room
# light does, and the specular lobe the ring existed to dodge never appeared at
# 40 mm. See nixos-private/docs/zaehler.md.
#
# Its seat and the band that carries it stay in the barrel, unused. Both tubes
# are printed already, and reshaping the barrel would scrap them to delete a
# groove that costs nothing.

# ---------------------------------------------------------------------------
# Cap
# ---------------------------------------------------------------------------
wu_cap = _cyl(WU_CAP_OD, WU_PLATE + WU_BAY, (0, 0, WU_WORK))
for _x, _y in _lugs():
    wu_cap = wu_cap.union(_cyl(WU_LUG_D, WU_PLATE, (_x, _y, WU_WORK)))
wu_cap = wu_cap.cut(_cyl(WU_CAP_BORE, WU_BAY + 1, (0, 0, WU_WORK + WU_PLATE)))
wu_cap = wu_cap.cut(_cyl(WU_LENS_D, WU_PLATE + 2, (0, 0, WU_WORK - 1)))
for _x, _y in _lugs():
    wu_cap = wu_cap.cut(_cyl(WU_CLEAR, WU_PLATE + 2, (_x, _y, WU_WORK - 1)))

# Camera boss, standing off the back of the lens plate: a square pocket for the
# lens holder, then a wider relief so the module's own PCB has somewhere to sit.
wu_cap = wu_cap.union(
    _box(WU_BOSS, WU_BOSS, WU_MOD_L + 3.0, (0, 0, WU_WORK + WU_PLATE))
)
wu_cap = wu_cap.cut(
    _box(WU_MOD_SQ, WU_MOD_SQ, WU_MOD_L, (0, 0, WU_WORK + WU_PLATE))
)
wu_cap = wu_cap.cut(
    _box(WU_MOD_PCB, WU_MOD_PCB, 4.0, (0, 0, WU_WORK + WU_PLATE + WU_MOD_L))
)

# Two posts and the bar that goes on them.
for sx in (-1, 1):
    wu_cap = wu_cap.union(
        _cyl(WU_POST_D, WU_POST_H, (sx * WU_POST_R, 0, WU_WORK + WU_PLATE))
    )
for sx in (-1, 1):
    wu_cap = wu_cap.cut(
        _cyl(WU_PILOT, WU_POST_H + 1,
             (sx * WU_POST_R, 0, WU_WORK + WU_PLATE - 0.5))
    )

# Screw access: one channel per lug, through the wall, all the way up.
for _x, _y in _lugs():
    wu_cap = wu_cap.cut(
        _cyl(WU_LUG_ACCESS, WU_BAY + 2, (_x, _y, WU_WORK + WU_PLATE))
    )

# USB window, open to the back edge.
wu_cap = wu_cap.cut(
    _box(2 * WU_USB_X, WU_CAP_OD, WU_BAY,
         (0, -WU_CAP_OD / 2 - 20.0, WU_USB_Z0))
)


display(wu_cap)
_export(wu_cap, "wasserzaehler_cap")


# ---------------------------------------------------------------------------
# wasserzaehler_bar -- what actually holds the board down
# ---------------------------------------------------------------------------
# A flat strip across the two posts. It is a separate part because it has to
# come off to get the board out, and it prints flat with nothing overhead.
wu_bar = _box(WU_BAR_L, WU_BAR_W, WU_BAR_T).edges("|Z").fillet(3.0)
for sx in (-1, 1):
    wu_bar = wu_bar.cut(_cyl(WU_CLEAR, WU_BAR_T + 2, (sx * WU_POST_R, 0, -1)))

display(wu_bar)
_export(wu_bar, "wasserzaehler_bar")

print("wasserzaehler tube %.1f cm3  strap %.1f cm3  cap %.1f cm3" % (
    wu_tube.val().Volume() / 1000.0,
    wu_strap.val().Volume() / 1000.0,
    wu_cap.val().Volume() / 1000.0))
print("wasserzaehler bar %.1f cm3" % (wu_bar.val().Volume() / 1000.0,))
