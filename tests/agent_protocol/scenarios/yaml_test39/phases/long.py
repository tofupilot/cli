import time


def long(log):
    for i in range(20):
        log.info(f"tick {i}")
        time.sleep(0.5)
