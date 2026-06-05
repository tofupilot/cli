import os
import tempfile


def flaky(log):
    # Use a file-based counter so the state survives across worker invocations.
    state = os.path.join(tempfile.gettempdir(), "yaml_test29_count.txt")
    try:
        with open(state) as f:
            n = int(f.read() or "0")
    except FileNotFoundError:
        n = 0
    n += 1
    with open(state, "w") as f:
        f.write(str(n))
    log.info(f"attempt {n}")
    if n < 3:
        return "retry"
    return None
