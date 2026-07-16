class PowerSupply:
    def __init__(self, initial_voltage=0.0):
        # config from procedure.yaml arrives as a kwarg; each plug key gets
        # its own value, proving per-instance config injection.
        self.voltage = initial_voltage

    def read_voltage(self):
        return self.voltage
