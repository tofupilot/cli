import time


def current_draw(measurements, log):
    log.info("starting current draw")
    time.sleep(1.2)
    measurements.i_idle = 32.5
    log.info("i_idle captured")
    time.sleep(1.5)
    measurements.i_load = 412.8
    log.info("i_load captured")
    time.sleep(1.5)
    measurements.i_peak = 690.1
    log.info("i_peak captured")
