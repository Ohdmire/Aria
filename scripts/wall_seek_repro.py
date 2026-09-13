"""wall 进程 seek 复现驱动:加载 soranosita (allein's Extra),8s 后 seek 到
310s(无 note 尾段,音频 323.16s),观察 ended 事件与 status 时间轴。"""
import json, subprocess, sys, threading, time

OSU = r"D:\osu\files\a\a2\a2bb34c01f8c444562b7e6499aa47d1d75f3f213a45a7360cc1c79a2390d1178"
pc = json.load(open(r'C:/Users/ATRI1/AppData/Roaming/com.ohdmire.aria/path-cache.json', encoding='utf-8'))
manifest = pc['9464a2ba22a04420a463807b522b341b']['manifest']

p = subprocess.Popen(
    [r'D:\Github\Aria\target\debug\aria.exe', '--wallpaper'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    text=True, encoding='utf-8', errors='replace',
)
t0 = time.time()
last_status = {}

def send(obj):
    p.stdin.write(json.dumps(obj) + '\n'); p.stdin.flush()

def reader():
    for line in p.stdout:
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except Exception:
            print(f"[{time.time()-t0:6.1f}] RAW {line[:120]}")
            continue
        e = ev.get('event')
        if e == 'status':
            last_status.update(ev)
            print(f"[{time.time()-t0:6.1f}] STATUS t={ev['tMs']/1000:8.2f}s dur={ev['durationMs']/1000:8.2f}s playing={ev['playing']} loop={ev['looping']}")
        else:
            print(f"[{time.time()-t0:6.1f}] {ev}")

def err_reader():
    for line in p.stderr:
        print(f"[{time.time()-t0:6.1f}] ERR {line.rstrip()[:160]}")

threading.Thread(target=reader, daemon=True).start()
threading.Thread(target=err_reader, daemon=True).start()

send({"cmd": "set_ffmpeg_bins", "ffmpeg": r"D:\Github\Aria\bin\ffmpeg.exe", "ffprobe": r"D:\Github\Aria\bin\ffprobe.exe"})
send({"cmd": "set_master", "v": 0.0})
send({"cmd": "set_volume", "v": 0.0})
send({"cmd": "load", "path": OSU, "diff": None, "speed": 1.0, "start": 0.0,
      "loop_playback": False, "manifest": manifest, "skin": None,
      "force_colours": False, "hidden": False, "mods": 0,
      "storyboard": True, "video": True, "beatmap_hitsounds": True})
print("loaded cmd sent; waiting 8s...")
time.sleep(8)
print(">>> SEEK to 310000ms (note-less tail; audio ends 323161ms)")
send({"cmd": "seek", "ms": 310000.0})
# 310 → 323.16 = 13.2s of audio;观察 25s 覆盖自然结尾 + 余量
for _ in range(25):
    time.sleep(1)
    send({"cmd": "status"})
print(">>> sending quit")
send({"cmd": "quit"})
time.sleep(2)
p.kill()
