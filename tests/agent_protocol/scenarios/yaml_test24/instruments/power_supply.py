class PowerSupply:
    def __init__(self):
        self.voltage = 0.0

    def set_voltage(self, v):
        self.voltage = v

    def read_voltage(self):
        return self.voltage
