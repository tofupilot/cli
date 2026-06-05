def check(log, unit):
    log.info(f"unit sn={unit.serial_number} sub_units={getattr(unit, 'sub_units', None)}")
