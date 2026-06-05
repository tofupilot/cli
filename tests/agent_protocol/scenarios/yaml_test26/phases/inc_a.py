def inc_a(log, counter):
    counter.inc()
    log.info(f"A: n={counter.n}")
