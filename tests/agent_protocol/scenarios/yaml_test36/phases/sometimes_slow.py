import os
import time
import tempfile


def sometimes_slow(log):
    state = os.path.join(tempfile.gettempdir(), "yaml_test36_count.txt")
    try:
        with open(state) as f:
            n = int(f.read() or "0")
    except FileNotFoundError:
        n = 0
    n += 1
    with open(state, "w") as f:
        f.write(str(n))
    log.info(f"attempt {n}")
    if n < 2:
        time.sleep(5)  # first attempt times out
    log.info("succeeded")
