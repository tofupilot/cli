import time


def voltage_sweep(measurements, log):
    log.info("starting voltage sweep")
    time.sleep(1.2)
    measurements.v_idle = 5.01
    log.info("v_idle captured")
    time.sleep(1.5)
    measurements.v_load = 4.82
    log.info("v_load captured")
    time.sleep(1.5)
    measurements.v_peak = 5.34
    log.info("v_peak captured")
