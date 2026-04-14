// Swift helper for microphone activity / selection heuristics.
//
// Default mode:
//   Prints "1" if any audio input device is currently active, "0" otherwise.
//
// --active-device:
//   Prints the input-device name when exactly one input device is active.
//   Prints an empty line when no input is active or when the result is ambiguous.

import CoreAudio
import Foundation

private func defaultInputDeviceID() -> AudioObjectID? {
    var deviceID = AudioObjectID(kAudioObjectUnknown)
    var property = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain
    )
    var size = UInt32(MemoryLayout<AudioObjectID>.size)
    let status = AudioObjectGetPropertyData(
        AudioObjectID(kAudioObjectSystemObject),
        &property,
        0,
        nil,
        &size,
        &deviceID
    )
    guard status == noErr, deviceID != AudioObjectID(kAudioObjectUnknown) else {
        return nil
    }
    return deviceID
}

private func allAudioDeviceIDs() -> [AudioObjectID] {
    var property = AudioObjectPropertyAddress(
        mSelector: kAudioHardwarePropertyDevices,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain
    )
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(
        AudioObjectID(kAudioObjectSystemObject),
        &property,
        0,
        nil,
        &size
    ) == noErr else {
        return []
    }

    let count = Int(size) / MemoryLayout<AudioObjectID>.size
    var deviceIDs = Array(repeating: AudioObjectID(kAudioObjectUnknown), count: count)
    guard AudioObjectGetPropertyData(
        AudioObjectID(kAudioObjectSystemObject),
        &property,
        0,
        nil,
        &size,
        &deviceIDs
    ) == noErr else {
        return []
    }
    return deviceIDs.filter { $0 != AudioObjectID(kAudioObjectUnknown) }
}

private func deviceName(_ deviceID: AudioObjectID) -> String? {
    var property = AudioObjectPropertyAddress(
        mSelector: kAudioObjectPropertyName,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain
    )
    var size = UInt32(MemoryLayout<CFString?>.size)
    var cfName: CFString?
    let status = AudioObjectGetPropertyData(deviceID, &property, 0, nil, &size, &cfName)
    guard status == noErr, let cfName else {
        return nil
    }
    return cfName as String
}

private func deviceHasInput(_ deviceID: AudioObjectID) -> Bool {
    var property = AudioObjectPropertyAddress(
        mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: kAudioDevicePropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain
    )
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(deviceID, &property, 0, nil, &size) == noErr else {
        return false
    }
    let raw = UnsafeMutableRawPointer.allocate(
        byteCount: Int(size),
        alignment: MemoryLayout<AudioBufferList>.alignment
    )
    defer { raw.deallocate() }

    guard AudioObjectGetPropertyData(deviceID, &property, 0, nil, &size, raw) == noErr else {
        return false
    }

    let bufferList = raw.assumingMemoryBound(to: AudioBufferList.self)
    let buffers = UnsafeMutableAudioBufferListPointer(bufferList)
    return buffers.contains { $0.mNumberChannels > 0 }
}

private func deviceIsRunning(_ deviceID: AudioObjectID) -> Bool {
    var running: UInt32 = 0
    var property = AudioObjectPropertyAddress(
        mSelector: kAudioDevicePropertyDeviceIsRunningSomewhere,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain
    )
    var size = UInt32(MemoryLayout<UInt32>.size)
    let status = AudioObjectGetPropertyData(deviceID, &property, 0, nil, &size, &running)
    return status == noErr && running > 0
}

private func activeInputDeviceNames() -> [String] {
    allAudioDeviceIDs()
        .filter { deviceHasInput($0) && deviceIsRunning($0) }
        .compactMap(deviceName)
}

private func preferredActiveInputDeviceName() -> String? {
    let activeNames = activeInputDeviceNames()
    guard !activeNames.isEmpty else {
        return nil
    }

    let uniqueNames = Array(Set(activeNames)).sorted()
    if uniqueNames.count == 1 {
        return uniqueNames[0]
    }

    if let defaultID = defaultInputDeviceID(),
       let defaultName = deviceName(defaultID),
       uniqueNames.contains(defaultName)
    {
        return defaultName
    }

    return nil
}

@main
struct MicCheckMain {
    static func main() {
        if CommandLine.arguments.contains("--active-device") {
            print(preferredActiveInputDeviceName() ?? "")
        } else {
            print(activeInputDeviceNames().isEmpty ? "0" : "1")
        }
    }
}
