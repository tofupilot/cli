def check(log, measurements, counter):
    v = counter.value()
    measurements.total = v
    log.info(f"total: {v}")
