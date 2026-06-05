def cfg(log, power_supply):
    power_supply.set_voltage(5.0)
    log.info("configured")
