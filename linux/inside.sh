#!/bin/bash
# The part of test.sh that runs inside a D-Bus session.
B=/target/release/bottle-eq
pipewire >/tmp/pipewire.log 2>&1 &
sleep 1
wireplumber >/tmp/wireplumber.log 2>&1 &
sleep 2
pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=speakers node.description="Speakers (to nowhere)" media.class=Audio/Sink object.linger=true audio.position=[FL FR] }' >/dev/null
sleep 1
echo "== devices"
$B devices
echo "== selftest"
$B selftest --to speakers
echo "== playtest, flat"
$B playtest --to speakers --file test/pink.wav --level 0.5 --record test/linux/flat
echo "== playtest, ten bands"
$B playtest --to speakers --file test/pink.wav --level 0.5 --gains 6,0,-4,0,3,-6,0,4,0,-3 --record test/linux/bands
echo "== playtest, a mode and points"
$B playtest --to speakers --file test/pink.wav --level 0.5 --mode "bass boost" --points "100:cut:1.5 1k:-2 12k:+8:spike" --record test/linux/points
echo "== measuring: a microphone that hears just what the speakers are given"
pw-loopback --capture-props='stream.capture.sink=true target.object=speakers node.name=hears' --playback-props='media.class=Audio/Source node.name=mic node.description="Microphone (hears the speakers)"' >/tmp/loopback.log 2>&1 &
sleep 1
$B devices
$B measure --mic mic --to speakers --keep test/linux/measured_flat.csv | tail -3
$B measure --mic mic --to speakers --gains 6,0,-4,0,3,-6,0,4,0,-3 --keep test/linux/measured_bands.csv | tail -3
echo "== correcting: the way is flat here, so whatever the equalizer is set to while measuring, the correction must be nothing"
$B measure --mic mic --to speakers --gains 0,0,5,2,0,-1,0,0,-3,-6 --correct --keep test/linux/correct.csv | grep -E "believed|the correction|bands|points|From flat|kept"
$B calibrations
echo "== the window"
Xvfb :99 -screen 0 1280x800x24 >/tmp/xvfb.log 2>&1 &
sleep 1
unset WAYLAND_DISPLAY
DISPLAY=:99 LIBGL_ALWAYS_SOFTWARE=1 timeout 60 $B window --to speakers --mode rock --file test/music.wav --level 0.5 --drag 3:10k:+8 --shot test/linux/window.bmp --seconds 7
ls -la test/linux/window.bmp
echo "== the graph while it ran"
pw-cli ls Node 2>/dev/null | grep -E "node.name|media.class" | head -20
