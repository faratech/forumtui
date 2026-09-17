#!/usr/bin/env python3
"""Drive wftui in a PTY against the real forum and record a demo GIF.

    python3 docs/demo/make_demo_gif.py

Seeds a scratch config dir from the operator's (so the real token is
used but the demo never writes drafts or read-marks into the real
store), runs the client in a 104x28 PTY, feeds it a scripted key
sequence, snapshots the screen with pyte after every action, renders
the snapshots with Pillow + DejaVu Sans Mono, and writes
docs/demo/wftui-demo.gif. Frames are deduplicated with accumulated
delays, so still scenes cost one frame each and typing animates.
"""

import hashlib
import json
import os
import pty
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time
import fcntl

import pyte
from PIL import Image, ImageDraw, ImageFont

COLS, ROWS = 104, 28
FONT_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
FONT_SIZE = 14
OUT = os.path.join(os.path.dirname(__file__), "wftui-demo.gif")
BIN = os.path.join(os.path.dirname(__file__), "..", "..", "bin", "wftui")
REAL_CFG = os.path.expanduser("~/.config/wftui")
DEMO_CFG = "/tmp/wftui-demo-cfg"

BG = (18, 20, 24)
FG = (200, 202, 206)
CURSOR = (120, 180, 255)

# ---- the script: (keys, seconds to settle) --------------------------------
# Plain chars go as-is; \r Enter, \x1b Esc, \x0b Ctrl+K, \t Tab.
SCENES = [
    (None, 3.0),          # bootstrap: Home tree + first thread page fill in
    ("j", 1.2),           # walk the forum tree
    ("j", 1.0),
    ("j", 1.0),
    ("\r", 2.0),          # open the selected forum; the list fills
    ("j", 0.7),           # row 2: the 231-reply desktop thread
    ("\r", 2.5),          # open it; the first page renders
    ("j", 0.9),           # scroll
    ("j", 0.9),
    ("j", 0.9),
    ("n", 1.2),           # next post (the accent gutter moves)
    ("n", 1.2),
    ("n", 1.2),
    ("r", 1.0),           # composer
    ("\x18", 0.7),        # ^X: discard whatever the relay resumed, start clean
]
REPLY = "Thanks — the group policy change fixed it for me.\n"
for ch in REPLY:
    SCENES.append((ch, 0.08))
SCENES += [
    (None, 0.9),
    ("\x1b", 1.2),        # Esc: draft saved (to the scratch dir only)
    ("\x0b", 0.8),        # Ctrl+K: the go-to palette
    ("a", 0.25),
    ("l", 0.25),
    ("e", 0.7),           # "ale" — the palette narrows to Alerts
    ("\r", 1.6),          # open it
    ("q", 0.8),           # quit
]


def seed_config():
    if os.path.exists(DEMO_CFG):
        shutil.rmtree(DEMO_CFG)
    os.makedirs(DEMO_CFG)
    # token + site config only — never drafts.json: the demo must not show
    # (or append to) the operator's real unsent writing.
    for name in ("token.json", "config.json"):
        src = os.path.join(REAL_CFG, name)
        if os.path.exists(src):
            shutil.copy2(src, os.path.join(DEMO_CFG, name))


def restore_token():
    """The capture may have silently refreshed: keep the newest grant."""
    src = os.path.join(DEMO_CFG, "token.json")
    dst = os.path.join(REAL_CFG, "token.json")
    if os.path.exists(src):
        a = open(src, "rb").read()
        b = open(dst, "rb").read() if os.path.exists(dst) else b""
        if a != b:
            tmp = dst + ".new"
            open(tmp, "wb").write(a)
            os.chmod(tmp, 0o600)
            os.replace(tmp, dst)
            print("token refreshed in place", file=sys.stderr)


class Driver:
    def __init__(self):
        self.master, slave = pty.openpty()
        winsize = struct.pack("HHHH", ROWS, COLS, 0, 0)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, winsize)
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            COLORTERM="truecolor",
            WFTUI_CONFIG_DIR=DEMO_CFG,
            WFTUI_NO_UPDATE="1",
            WFTUI_GRAPHICS="none",
        )
        self.proc = subprocess.Popen(
            [os.path.abspath(BIN)],
            stdin=slave, stdout=slave, stderr=slave,
            env=env, close_fds=True,
        )
        os.close(slave)
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.output = b""

    def pump(self, seconds):
        """Read the PTY for `seconds`, feeding pyte as bytes arrive."""
        end = time.monotonic() + seconds
        while True:
            left = end - time.monotonic()
            if left <= 0:
                break
            r, _, _ = select.select([self.master], [], [], min(left, 0.05))
            if r:
                try:
                    data = os.read(self.master, 65536)
                except OSError:
                    return False
                if not data:
                    return False
                self.output += data
                self.stream.feed(data)
        return self.proc.poll() is None

    def send(self, keys):
        os.write(self.master, keys.encode())

    def close(self):
        try:
            self.proc.terminate()
        except ProcessLookupError:
            pass
        try:
            self.proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        os.close(self.master)


class Renderer:
    def __init__(self):
        self.font = ImageFont.truetype(FONT_PATH, FONT_SIZE)
        self.bold = ImageFont.truetype(FONT_BOLD, FONT_SIZE)
        probe = self.font.getbbox("X")
        self.cw = probe[2] - probe[0]
        self.ch = FONT_SIZE + 4
        self.cache = {}

    def frame(self, screen):
        img = Image.new("RGB", (COLS * self.cw, ROWS * self.ch), BG)
        draw = ImageDraw.Draw(img)
        for y in range(ROWS):
            for x in range(COLS):
                cell = screen.buffer[y][x]
                ch = cell.data
                if not ch or ch == " ":
                    # still paint the background: reverse/default vary
                    bg = self.color(cell.bg, BG, fg=False)
                    if bg != BG:
                        draw.rectangle(
                            [x * self.cw, y * self.ch, (x + 1) * self.cw - 1, (y + 1) * self.ch - 1],
                            fill=bg,
                        )
                    continue
                fg = self.color(cell.fg, FG)
                bg = self.color(cell.bg, BG, fg=False)
                if cell.reverse:
                    fg, bg = bg, fg
                draw.rectangle(
                    [x * self.cw, y * self.ch, (x + 1) * self.cw - 1, (y + 1) * self.ch - 1],
                    fill=bg,
                )
                f = self.bold if cell.bold and cell.fg != "default" else self.font
                if f not in self.cache:
                    self.cache[f] = True
                draw.text((x * self.cw, y * self.ch + 1), ch, font=f, fill=fg)
        # the terminal cursor, wherever the app left it
        cx, cy = screen.cursor.x, screen.cursor.y
        if 0 <= cx < COLS and 0 <= cy < ROWS:
            draw.rectangle(
                [cx * self.cw, cy * self.ch, (cx + 1) * self.cw - 1, (cy + 1) * self.ch - 1],
                outline=CURSOR,
            )
        return img

    def color(self, spec, fallback, fg=True):
        if not spec or spec == "default":
            return fallback
        if isinstance(spec, str) and spec.startswith("#"):
            try:
                v = int(spec[1:], 16)
                return ((v >> 16) & 255, (v >> 8) & 255, v & 255)
            except ValueError:
                return fallback
        named = {
            "black": (0, 0, 0), "red": (205, 60, 60), "green": (78, 154, 6),
            "brown": (196, 160, 0), "blue": (52, 101, 164), "magenta": (117, 80, 123),
            "cyan": (6, 152, 154), "white": (211, 215, 207),
            "bright_black": (85, 87, 83), "bright_red": (239, 41, 41),
            "bright_green": (138, 226, 52), "bright_brown": (252, 233, 79),
            "bright_blue": (114, 159, 207), "bright_magenta": (173, 127, 168),
            "bright_cyan": (52, 226, 226), "bright_white": (238, 238, 236),
        }
        return named.get(spec, fallback)


def cleanup_draft():
    """One unrecorded session: navigate to the same composer, ^X the demo
    draft away, close. The relay forgets it, so the operator's own next
    reply to that thread starts clean."""
    idx = SCENES.index(("r", 1.0))
    drv = Driver()
    try:
        for keys, wait in SCENES[:idx + 1]:
            if keys:
                if not drv.pump(0.15):
                    return
                drv.send(keys)
            if not drv.pump(wait):
                return
        drv.send("\x18")  # ^X: discard the resumed (demo) draft
        drv.pump(0.8)
        drv.send("\x1b")  # close the composer; an empty body saves nothing
        drv.pump(1.0)
    finally:
        drv.close()
    restore_token()


def main():
    seed_config()
    drv = Driver()
    rnd = Renderer()
    frames = []  # (image bytes hash, PIL image, duration ms)
    last_hash = None
    try:
        for keys, wait in SCENES:
            if keys:
                if not drv.pump(0.15):
                    break
                drv.send(keys)
            alive = drv.pump(wait)
            img = rnd.frame(drv.screen)
            h = hashlib.sha1(img.tobytes()).hexdigest()
            if h != last_hash:
                frames.append([img, 0])
                last_hash = h
            else:
                if frames:
                    frames[-1][1] += int(wait * 1000)
                else:
                    frames.append([img, 0])
            if not alive:
                break
    finally:
        drv.close()
    restore_token()

    if not frames:
        sys.exit("no frames captured")
    # A post-quit frame is the restored terminal: drop it if it went blank.
    last = frames[-1][0]
    colors = last.getcolors(maxcolors=1 << 24)
    total = last.width * last.height
    bg_pixels = sum(c for c, col in colors if col == BG)
    if bg_pixels > 0.9 * total:
        frames.pop()
    pil_frames = []
    durations = []
    for img, extra in frames:
        pil_frames.append(img)
        durations.append(max(120, min(1500, 300 + extra)))
    pil_frames[0].save(
        OUT, save_all=True, append_images=pil_frames[1:],
        duration=durations, loop=0, optimize=True,
    )
    print(f"{len(pil_frames)} frames -> {OUT} ({os.path.getsize(OUT)} bytes)")
    cleanup_draft()


if __name__ == "__main__":
    main()
