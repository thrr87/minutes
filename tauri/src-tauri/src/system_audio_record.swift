import AVFoundation
import CoreGraphics
import CoreMedia
import Dispatch
import Foundation
import ScreenCaptureKit

private struct PermissionProbe: Encodable {
    let screenRecording: Bool
    let microphone: String
}

private func findMicCheckHelperURL() -> URL? {
    let fm = FileManager.default
    let currentExe = URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL
    let bundled = currentExe.deletingLastPathComponent().appendingPathComponent("mic_check")
    if fm.isExecutableFile(atPath: bundled.path) {
        return bundled
    }

    let sourceHelper = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()
        .deletingLastPathComponent()
        .appendingPathComponent("bin/mic_check")
    if fm.isExecutableFile(atPath: sourceHelper.path) {
        return sourceHelper
    }

    return nil
}

private func preferredActiveInputDeviceName() -> String? {
    guard let helperURL = findMicCheckHelperURL() else {
        return nil
    }

    let process = Process()
    process.executableURL = helperURL
    process.arguments = ["--active-device"]
    let stdout = Pipe()
    process.standardOutput = stdout
    process.standardError = Pipe()

    do {
        try process.run()
    } catch {
        return nil
    }
    process.waitUntilExit()

    guard process.terminationStatus == 0 else {
        return nil
    }

    let data = stdout.fileHandleForReading.readDataToEndOfFile()
    let name = String(data: data, encoding: .utf8)?
        .trimmingCharacters(in: .whitespacesAndNewlines)
    return (name?.isEmpty == false) ? name : nil
}

private func defaultInputDeviceName() -> String? {
    guard let helperURL = findMicCheckHelperURL() else {
        return nil
    }

    let process = Process()
    process.executableURL = helperURL
    process.arguments = ["--default-device"]
    let stdout = Pipe()
    process.standardOutput = stdout
    process.standardError = Pipe()

    do {
        try process.run()
    } catch {
        return nil
    }
    process.waitUntilExit()

    guard process.terminationStatus == 0 else {
        return nil
    }

    let data = stdout.fileHandleForReading.readDataToEndOfFile()
    let name = String(data: data, encoding: .utf8)?
        .trimmingCharacters(in: .whitespacesAndNewlines)
    return (name?.isEmpty == false) ? name : nil
}

private func requestedMicrophoneName() -> String? {
    guard let flagIndex = CommandLine.arguments.firstIndex(of: "--microphone-name"),
          flagIndex + 1 < CommandLine.arguments.count else {
        return nil
    }

    let name = CommandLine.arguments[flagIndex + 1]
        .trimmingCharacters(in: .whitespacesAndNewlines)
    return name.isEmpty ? nil : name
}

private func deviceMatchesPreferredName(_ device: AVCaptureDevice, preferredName: String) -> Bool {
    let lhs = device.localizedName.lowercased()
    let rhs = preferredName.lowercased()
    return lhs == rhs || lhs.contains(rhs) || rhs.contains(lhs)
}

private func findAudioDevice(preferredName: String) -> AVCaptureDevice? {
    AVCaptureDevice.devices(for: .audio).first(where: {
        deviceMatchesPreferredName($0, preferredName: preferredName)
    })
}

private func preferredMicrophoneDevice() -> AVCaptureDevice? {
    if let requestedName = requestedMicrophoneName(),
       let requestedDevice = findAudioDevice(preferredName: requestedName)
    {
        fputs("using configured microphone for call capture: \(requestedDevice.localizedName)\n", stderr)
        return requestedDevice
    }

    if let preferredName = preferredActiveInputDeviceName(),
       let preferredDevice = findAudioDevice(preferredName: preferredName)
    {
        fputs("using active microphone for call capture: \(preferredDevice.localizedName)\n", stderr)
        return preferredDevice
    }

    if let defaultName = defaultInputDeviceName(),
       let defaultDevice = findAudioDevice(preferredName: defaultName)
    {
        fputs("using system default microphone for call capture: \(defaultDevice.localizedName)\n", stderr)
        return defaultDevice
    }

    if let fallback = AVCaptureDevice.default(for: .audio) {
        fputs("falling back to AVFoundation default microphone for call capture: \(fallback.localizedName)\n", stderr)
        return fallback
    }

    return nil
}

@available(macOS 15.0, *)
private func hasScreenRecordingAccess() async -> Bool {
    if CGPreflightScreenCaptureAccess() {
        return true
    }

    do {
        _ = try await SCShareableContent.excludingDesktopWindows(
            false,
            onScreenWindowsOnly: true
        )
        return true
    } catch {
        return false
    }
}

@available(macOS 15.0, *)
private func requestScreenRecordingAccessIfNeeded() async -> Bool {
    if await hasScreenRecordingAccess() {
        return true
    }

    _ = CGRequestScreenCaptureAccess()

    for _ in 0..<20 {
        if await hasScreenRecordingAccess() {
            return true
        }
        try? await Task.sleep(nanoseconds: 250_000_000)
    }

    return false
}

private func microphoneAuthorizationLabel() -> String {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized:
        return "authorized"
    case .denied:
        return "denied"
    case .restricted:
        return "restricted"
    case .notDetermined:
        return "notDetermined"
    @unknown default:
        return "restricted"
    }
}

@available(macOS 15.0, *)
private func emitPermissionProbeAndExit() async {
    let probe = PermissionProbe(
        screenRecording: await hasScreenRecordingAccess(),
        microphone: microphoneAuthorizationLabel()
    )

    do {
        let data = try JSONEncoder().encode(probe)
        if let json = String(data: data, encoding: .utf8) {
            print(json)
            fflush(stdout)
            exit(0)
        }
    } catch {
        fputs("probe failed: \(error)\n", stderr)
    }

    exit(1)
}

@available(macOS 15.0, *)
final class NativeCallRecorder: NSObject, SCRecordingOutputDelegate, SCStreamOutput {
    private var stream: SCStream?
    private var recordingOutput: SCRecordingOutput?
    private let outputURL: URL
    private let sampleQueue = DispatchQueue(label: "minutes.system-audio.samples")
    private var monitorTimer: DispatchSourceTimer?
    private var lastSystemAudioSampleAt: Date?
    private var lastMicSampleAt: Date?
    private var lastReportedSystemLive = false
    private var lastReportedMicLive = false
    private var latestSystemLevel: UInt32 = 0
    private var latestMicLevel: UInt32 = 0

    // Per-source stem writers
    private var voiceStemFile: AVAudioFile?
    private var systemStemFile: AVAudioFile?
    private var voiceStemURL: URL?
    private var systemStemURL: URL?

    init(outputURL: URL) {
        self.outputURL = outputURL
    }

    private func ensureMicrophonePermission() async throws {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            return
        case .notDetermined:
            let granted = await withCheckedContinuation { continuation in
                AVCaptureDevice.requestAccess(for: .audio) { granted in
                    continuation.resume(returning: granted)
                }
            }
            guard granted else {
                throw NSError(
                    domain: "MinutesSystemAudioRecord",
                    code: 2,
                    userInfo: [
                        NSLocalizedDescriptionKey:
                            "Microphone access is required to capture your side of the call. Enable it in System Settings > Privacy & Security > Microphone."
                    ]
                )
            }
        case .denied:
            throw NSError(
                domain: "MinutesSystemAudioRecord",
                code: 2,
                userInfo: [
                    NSLocalizedDescriptionKey:
                        "Microphone access is turned off for Minutes. Enable it in System Settings > Privacy & Security > Microphone."
                ]
            )
        case .restricted:
            throw NSError(
                domain: "MinutesSystemAudioRecord",
                code: 2,
                userInfo: [
                    NSLocalizedDescriptionKey:
                        "Microphone access is restricted by macOS on this Mac, so Minutes cannot capture your side of the call."
                ]
            )
        @unknown default:
            throw NSError(
                domain: "MinutesSystemAudioRecord",
                code: 2,
                userInfo: [
                    NSLocalizedDescriptionKey:
                        "Microphone access is in an unknown state. Check System Settings > Privacy & Security > Microphone."
                ]
            )
        }
    }

    func start() async throws {
        guard await requestScreenRecordingAccessIfNeeded() else {
            throw NSError(
                domain: "MinutesSystemAudioRecord",
                code: 2,
                userInfo: [
                    NSLocalizedDescriptionKey:
                        "Screen & System Audio Recording access is required to capture call audio. Turn Minutes on in System Settings > Privacy & Security > Screen & System Audio Recording, then try Record Call again."
                ]
            )
        }

        try await ensureMicrophonePermission()

        let shareableContent = try await SCShareableContent.excludingDesktopWindows(
            false,
            onScreenWindowsOnly: true
        )
        guard let display = shareableContent.displays.first else {
            throw NSError(
                domain: "MinutesSystemAudioRecord",
                code: 1,
                userInfo: [NSLocalizedDescriptionKey: "No display available for ScreenCaptureKit capture."]
            )
        }

        let filter = SCContentFilter(
            display: display,
            excludingApplications: [],
            exceptingWindows: []
        )

        let configuration = SCStreamConfiguration()
        configuration.width = 2
        configuration.height = 2
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 2)
        configuration.queueDepth = 3
        configuration.capturesAudio = true
        configuration.captureMicrophone = true
        configuration.excludesCurrentProcessAudio = true
        configuration.showsCursor = false

        if let microphone = preferredMicrophoneDevice() {
            configuration.microphoneCaptureDeviceID = microphone.uniqueID
        }

        let stream = SCStream(filter: filter, configuration: configuration, delegate: nil)
        try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: sampleQueue)
        try stream.addStreamOutput(self, type: .microphone, sampleHandlerQueue: sampleQueue)
        let recordingConfiguration = SCRecordingOutputConfiguration()
        recordingConfiguration.outputURL = outputURL
        recordingConfiguration.outputFileType = .mov
        recordingConfiguration.videoCodecType = .h264

        let recordingOutput = SCRecordingOutput(
            configuration: recordingConfiguration,
            delegate: self
        )

        try stream.addRecordingOutput(recordingOutput)

        // Derive stem paths BEFORE startCapture to avoid race with early samples
        let baseName = outputURL.deletingPathExtension().lastPathComponent
        let stemDir = outputURL.deletingLastPathComponent()
        voiceStemURL = stemDir.appendingPathComponent("\(baseName).voice.wav")
        systemStemURL = stemDir.appendingPathComponent("\(baseName).system.wav")

        do {
            try await stream.startCapture()
        } catch {
            let nsError = error as NSError
            if nsError.domain == SCStreamErrorDomain,
               nsError.code == SCStreamError.failedToStartAudioCapture.rawValue {
                throw NSError(
                    domain: "MinutesSystemAudioRecord",
                    code: nsError.code,
                    userInfo: [
                        NSLocalizedDescriptionKey:
                            "Screen & System Audio Recording access is required to capture call audio. Turn Minutes on in System Settings > Privacy & Security > Screen & System Audio Recording, then try Record Call again."
                    ]
                )
            }
            throw error
        }

        startMonitoring()

        self.stream = stream
        self.recordingOutput = recordingOutput
    }

    func stop() async {
        // Flush and close stem files on the sample queue to serialize
        // with any in-flight writeStemSamples calls. Without this,
        // nil'ing on the main thread races with writes on sampleQueue.
        sampleQueue.sync {
            voiceStemFile = nil
            systemStemFile = nil
        }

        guard let stream else {
            exit(0)
        }

        do {
            try await stream.stopCapture()
        } catch {
            fputs("stopCapture failed: \(error)\n", stderr)
            exit(1)
        }
    }

    private func startMonitoring() {
        let timer = DispatchSource.makeTimerSource(queue: sampleQueue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(150))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let now = Date()
            let systemLive = self.lastSystemAudioSampleAt.map { now.timeIntervalSince($0) < 1.5 } ?? false
            let micLive = self.lastMicSampleAt.map { now.timeIntervalSince($0) < 1.5 } ?? false
            if !systemLive {
                self.latestSystemLevel = 0
            }
            if !micLive {
                self.latestMicLevel = 0
            }

            let shouldEmit = systemLive || micLive || systemLive != self.lastReportedSystemLive || micLive != self.lastReportedMicLive
            guard shouldEmit else { return }

            self.lastReportedSystemLive = systemLive
            self.lastReportedMicLive = micLive
            let payload: [String: Any] = [
                "event": "health",
                "call_audio_live": systemLive,
                "mic_live": micLive,
                "call_audio_level": self.latestSystemLevel,
                "mic_level": self.latestMicLevel
            ]
            if let data = try? JSONSerialization.data(withJSONObject: payload),
               let json = String(data: data, encoding: .utf8) {
                print(json)
                fflush(stdout)
            }
        }
        timer.resume()
        monitorTimer = timer
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of outputType: SCStreamOutputType) {
        guard CMSampleBufferIsValid(sampleBuffer), CMSampleBufferDataIsReady(sampleBuffer) else {
            return
        }
        let now = Date()
        switch outputType {
        case .audio:
            lastSystemAudioSampleAt = now
            writeStemSamples(sampleBuffer, source: .audio)
        case .microphone:
            lastMicSampleAt = now
            writeStemSamples(sampleBuffer, source: .microphone)
        default:
            break
        }
    }

    private func writeStemSamples(_ sampleBuffer: CMSampleBuffer, source: SCStreamOutputType) {
        guard let formatDescription = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(formatDescription)?.pointee else {
            return
        }

        guard let blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer) else {
            return
        }

        let sampleCount = CMSampleBufferGetNumSamples(sampleBuffer)
        guard sampleCount > 0 else { return }

        var lengthAtOffset: Int = 0
        var totalLength: Int = 0
        var dataPointer: UnsafeMutablePointer<Int8>?
        let status = CMBlockBufferGetDataPointer(blockBuffer, atOffset: 0, lengthAtOffsetOut: &lengthAtOffset, totalLengthOut: &totalLength, dataPointerOut: &dataPointer)
        guard status == kCMBlockBufferNoErr, let data = dataPointer else {
            return
        }

        let channelCount = Int(asbd.mChannelsPerFrame)
        let sampleRate = asbd.mSampleRate
        let isFloat = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0

        // Stems are always mono float32 — mix down if multi-channel
        guard let monoFormat = AVAudioFormat(
            commonFormat: .pcmFormatFloat32,
            sampleRate: sampleRate,
            channels: 1,
            interleaved: false
        ) else { return }

        // Lazily create the stem file on first samples
        let stemFile: AVAudioFile?
        switch source {
        case .microphone:
            if voiceStemFile == nil, let url = voiceStemURL {
                do {
                    voiceStemFile = try AVAudioFile(forWriting: url, settings: monoFormat.settings)
                } catch {
                    fputs("failed to create voice stem file: \(error)\n", stderr)
                }
            }
            stemFile = voiceStemFile
        case .audio:
            if systemStemFile == nil, let url = systemStemURL {
                do {
                    systemStemFile = try AVAudioFile(forWriting: url, settings: monoFormat.settings)
                } catch {
                    fputs("failed to create system stem file: \(error)\n", stderr)
                }
            }
            stemFile = systemStemFile
        default:
            return
        }

        guard let file = stemFile else { return }

        // Mix multi-channel source data to mono float32.
        // ScreenCaptureKit may deliver interleaved or non-interleaved audio.
        let isNonInterleaved = (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0
        let frameCount = AVAudioFrameCount(sampleCount)
        guard let pcmBuffer = AVAudioPCMBuffer(pcmFormat: monoFormat, frameCapacity: frameCount) else {
            return
        }
        pcmBuffer.frameLength = frameCount

        guard let monoPtr = pcmBuffer.floatChannelData?[0] else { return }
        let bytesPerSample = isFloat ? 4 : 2

        if isNonInterleaved {
            // Non-interleaved: each channel is a separate plane of `frameCount` samples.
            // The CMBlockBuffer contains them sequentially: [ch0 frames][ch1 frames]...
            let planeSize = Int(frameCount) * bytesPerSample
            for frame in 0..<Int(frameCount) {
                var sum: Float = 0.0
                for ch in 0..<channelCount {
                    let offset = ch * planeSize + frame * bytesPerSample
                    guard offset + bytesPerSample <= totalLength else { break }
                    if isFloat {
                        var val: Float = 0.0
                        memcpy(&val, data.advanced(by: offset), 4)
                        sum += val
                    } else {
                        var val: Int16 = 0
                        memcpy(&val, data.advanced(by: offset), 2)
                        sum += Float(val) / 32768.0
                    }
                }
                monoPtr[frame] = sum / Float(channelCount)
            }
        } else {
            // Interleaved: samples are [ch0 ch1 ch0 ch1 ...]
            for frame in 0..<Int(frameCount) {
                var sum: Float = 0.0
                for ch in 0..<channelCount {
                    let offset = (frame * channelCount + ch) * bytesPerSample
                    guard offset + bytesPerSample <= totalLength else { break }
                    if isFloat {
                        var val: Float = 0.0
                        memcpy(&val, data.advanced(by: offset), 4)
                        sum += val
                    } else {
                        var val: Int16 = 0
                        memcpy(&val, data.advanced(by: offset), 2)
                        sum += Float(val) / 32768.0
                    }
                }
                monoPtr[frame] = sum / Float(channelCount)
            }
        }

        var sumSquares: Float = 0
        for frame in 0..<Int(frameCount) {
            let sample = monoPtr[frame]
            sumSquares += sample * sample
        }
        let rms = sqrt(sumSquares / max(Float(frameCount), 1))
        let level = UInt32(min(100.0, max(0.0, Double(rms) * 2000.0)))
        switch source {
        case .microphone:
            latestMicLevel = level
        case .audio:
            latestSystemLevel = level
        default:
            break
        }

        do {
            try file.write(from: pcmBuffer)
        } catch {
            fputs("stem write failed: \(error)\n", stderr)
        }
    }

    func recordingOutputDidStartRecording(_ recordingOutput: SCRecordingOutput) {
        print("ready")
        fflush(stdout)

        // Report stem paths so the Rust side knows where to find them
        let stemInfo: [String: Any] = [
            "event": "stems",
            "voice_stem": voiceStemURL?.path ?? "",
            "system_stem": systemStemURL?.path ?? ""
        ]
        if let data = try? JSONSerialization.data(withJSONObject: stemInfo),
           let json = String(data: data, encoding: .utf8) {
            print(json)
            fflush(stdout)
        }
    }

    func recordingOutputDidFinishRecording(_ recordingOutput: SCRecordingOutput) {
        exit(0)
    }

    func recordingOutput(
        _ recordingOutput: SCRecordingOutput,
        didFailWithError error: Error
    ) {
        fputs("recordingOutput failed: \(error)\n", stderr)
        exit(1)
    }
}

@main
struct NativeCallRecordMain {
    // Keep the signal source alive after `run()` returns so the SIGTERM handler
    // remains installed for the lifetime of the helper.
    nonisolated(unsafe) static var retainedStopSource: DispatchSourceSignal?

    static func main() {
        Task {
            await run()
        }
        dispatchMain()
    }

    static func run() async {
        guard #available(macOS 15.0, *) else {
            fputs("ScreenCaptureKit recording output requires macOS 15.0 or newer.\n", stderr)
            exit(1)
        }

        if CommandLine.arguments.contains("--probe") {
            await emitPermissionProbeAndExit()
        }

        guard CommandLine.arguments.count >= 2 else {
            fputs("usage: system_audio_record <output.mov>\n", stderr)
            exit(1)
        }

        let outputURL = URL(fileURLWithPath: CommandLine.arguments[1])
        let recorder = NativeCallRecorder(outputURL: outputURL)

        signal(SIGTERM, SIG_IGN)
        let stopSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        stopSource.setEventHandler {
            Task {
                await recorder.stop()
            }
        }
        stopSource.resume()
        NativeCallRecordMain.retainedStopSource = stopSource

        do {
            try await recorder.start()
        } catch {
            let nsError = error as NSError
            fputs("start failed: \(nsError.localizedDescription)\n", stderr)
            exit(1)
        }
    }
}
