"""Architecture checks must reject a thin library labelled as universal."""
import struct
import unittest
from embedded_wheel import verify_macos_architecture


class MacWheelArchitecture(unittest.TestCase):
    def test_host_specific_tags(self):
        for architecture, cpu in [("arm64", 0x0100000C), ("x86_64", 0x01000007)]:
            binary = bytes.fromhex("cffaedfe") + struct.pack("<I", cpu)
            verify_macos_architecture(
                binary, f"vlite_embedded-0.1.0-py3-none-macosx_11_0_{architecture}.whl",
                f"Tag: py3-none-macosx_11_0_{architecture}\n")

    def test_wrong_or_universal_tag(self):
        binary = bytes.fromhex("cffaedfe") + struct.pack("<I", 0x0100000C)
        for architecture in ["x86_64", "universal2"]:
            with self.assertRaises(AssertionError):
                verify_macos_architecture(
                    binary, f"vlite_embedded-0.1.0-py3-none-macosx_11_0_{architecture}.whl",
                    f"Tag: py3-none-macosx_11_0_{architecture}\n")
        with self.assertRaises(AssertionError):
            verify_macos_architecture(binary, "test_arm64.whl", "Tag: py3-none-macosx_11_0_universal2\n")


if __name__ == "__main__":
    unittest.main()
