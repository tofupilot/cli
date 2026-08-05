def second_read(log, measurements, psu):
    measurements.init_count = psu.read_init_count()
    log.info("second read done")
