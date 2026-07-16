def measure(log, measurements, psu_a, psu_b):
    # no configure phase: the plugs came up pre-set from their config
    measurements.va = psu_a.read_voltage()
    measurements.vb = psu_b.read_voltage()
    log.info("measured")
