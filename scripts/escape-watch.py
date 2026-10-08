#!/usr/bin/env python3
"""Rapid poller: watch /api/exec for escape_active > 0 windows."""
import json, time, urllib.request

hits = 0
t0 = time.time()
last = 0
while time.time() - t0 < 45:
    try:
        with urllib.request.urlopen("http://localhost:3000/api/exec", timeout=1) as r:
            d = json.load(r)
        st = d["stats"]
        pos_esc = [p for p in d["positions"] if p.get("escape")]
        if st["escape_active"] > 0 or pos_esc:
            hits += 1
            p = pos_esc[0] if pos_esc else None
            e = p["escape"] if p else None
            print(f"{time.time()-t0:6.2f}s active={st['escape_active']} pos_esc={len(pos_esc)} "
                  f"phase={e['phase'] if e else '-'} reason={e['reason'] if e else '-'} "
                  f"venue={e['hedge_venue'] if e else '-'}")
        if st["escape_exits"] != last:
            print(f"{time.time()-t0:6.2f}s exits: {last} -> {st['escape_exits']}")
            last = st["escape_exits"]
    except Exception as ex:
        pass
    time.sleep(0.05)
print(f"done: {hits} active-window samples in 45s")
