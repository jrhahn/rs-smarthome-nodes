# Home Assistant

**Nothing here needs merging into `configuration.yaml` any more.**

Every node announces itself over retained MQTT discovery on its first connect
after a power-up — both directions:

- the **readings** become `sensor` entities (weight, temperature, humidity, CO₂,
  PM2.5/PM10);
- the **calibration and tuning knobs** become `number` / `switch` / `button`
  entities, filed under the device's *Configuration* section.

Adding a node to the fleet therefore needs no change on the Home Assistant side
at all. The only requirement is the MQTT integration with discovery enabled (the
default) on the `homeassistant/` prefix.

## What each node exposes

Controls are per node, so nothing dead shows up on a device card:

| Control | Entity | On which node |
| --- | --- | --- |
| `threshold` | number, g | nodes with a load cell (`terrasse`) |
| `scale_factor` | number | nodes with a load cell |
| `offset` | number | nodes with a load cell |
| `tare` | button | nodes with a load cell |
| `idle_interval` | number, s | battery nodes |
| `active_interval` | number, s | battery nodes |
| `heartbeat_interval` | number, s | battery nodes |
| `deep_sleep` | switch | battery nodes |

The mains nodes have no tunables today: their sampling cadence is a build-time
per-node value (`sample_secs` in `src/node.rs`) and they never sleep.

**Changes apply with a delay.** The controls publish **retained** to
`<namespace>/<node>/config/<key>`, and the firmware only reads that topic while
it is online for a publish — on the scale, the next bird visit or heartbeat. The
broker holds the value until then; the node persists it to flash once it has it.

The controls also *read their state back off their own command topic*, which is
why they show the last value you set even after a Home Assistant restart. They
show what was last commanded, which is not proof the node has picked it up yet.

## Calibrating the scale

Two points, in this order:

1. **Zero.** With the pan empty, press **Tarieren**. The node adopts its current
   empty baseline as `offset` on its next online cycle. (`offset` is also
   editable by hand, if you would rather type the raw HX711 value.)
2. **Span.** Put a known weight on the pan and wait for a reading. If it shows
   `w_shown` grams instead of the true `w_true`, multiply `scale_factor` by
   `w_shown / w_true` and set the result. Repeat once if it is still off.

`scale_factor` is raw HX711 ticks per gram, so it is only valid for the exact
mechanical mounting it was calibrated on — re-do the span after any change to
the load cell's fixture. The zero drifts with temperature and creep, so re-tare
occasionally; the firmware also tracks slow baseline drift by itself.

## Example dashboard card

Add to a dashboard (Raw configuration editor). Entity ids follow from the node
name Home Assistant assigns the device — adjust if yours differ.

```yaml
type: vertical-stack
cards:
  - type: entities
    title: Meisenknödel
    entities:
      - entity: sensor.terrasse_gewicht
      - entity: sensor.terrasse_temperatur
      - entity: sensor.terrasse_luft_temperatur
      - entity: sensor.terrasse_luft_luftfeuchtigkeit
  - type: entities
    title: Kalibrierung
    entities:
      - entity: button.terrasse_tarieren
      - entity: number.terrasse_kalibrierfaktor
      - entity: number.terrasse_tara_offset
      - entity: number.terrasse_ausloseschwelle
  - type: history-graph
    hours_to_show: 24
    entities:
      - entity: sensor.terrasse_gewicht
```

## Migrating from the hand-declared YAML

This directory used to hold a `configuration.yaml` fragment with the `mqtt:`
number/switch blocks and a `birdscale_tare` script. To move off it:

1. Delete that block (and the script) from your `configuration.yaml`, plus the
   old hand-declared `smarthome/terrasse/state` and `smarthome/terrasse/temperature` sensors,
   and restart Home Assistant. The old entities are gone; the discovered ones —
   with different entity ids — have already appeared.
2. Fix up any dashboard cards and automations that named the old entities.
3. The firmware's mirror of the weight to `birds/scale/state` is **already
   gone**: the node was renamed from `terrasse` to `terrasse` and moved to the
   fleet's own `smarthome/terrasse/…` namespace, which retired the whole `birds`
   prefix along with it. `legacy_weight_topic` in `src/node.rs` is now unused by
   every node.

The old tare script published a retained timestamp to
`birds/scale/config/tare`. That topic is no longer subscribed to — the node
listens on `smarthome/terrasse/config/#` now — so a leftover retained message
under the old prefix will sit on the broker until it is cleared by hand:

```bash
mosquitto_pub -h <broker> -t birds/scale/config/tare -r -n
```

The firmware still understands the tokens themselves on its current topic, and
clears the retained message once it has acted on it.

## Forcing a re-announce

Discovery is published once per power cycle (the flag lives in RTC RAM, which a
reflash clears). To force it otherwise, delete the retained configs and
power-cycle the board:

```bash
mosquitto_pub -h <broker-ip> -t 'homeassistant/sensor/<node>/<key>/config' -r -n
mosquitto_pub -h <broker-ip> -t 'homeassistant/number/<node>/<key>/config' -r -n
```
