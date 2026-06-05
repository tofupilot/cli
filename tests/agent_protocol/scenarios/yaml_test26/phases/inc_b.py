def inc_b(log, counter):
    counter.inc()
    log.info(f"B: n={counter.n}")
