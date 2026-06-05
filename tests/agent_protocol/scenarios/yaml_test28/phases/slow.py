import time


def slow(log):
    log.info("sleeping 5s, should be killed at 1s")
    time.sleep(5)
    log.info("never reached")
