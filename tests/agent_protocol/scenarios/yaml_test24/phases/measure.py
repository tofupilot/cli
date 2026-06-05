def measure(log, measurements, power_supply):
    measurements.v = power_supply.read_voltage()
    log.info("measured")
