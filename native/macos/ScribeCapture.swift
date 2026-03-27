import AVFoundation
import CoreGraphics
import CoreMedia
import Dispatch
import Foundation
import ScreenCaptureKit

struct DisplayInfo: Codable {
    let displayID: UInt32
    let width: Int
    let height: Int
    let isMain: Bool
}

struct DoctorOutput: Codable {
    let object: String
    let screenCaptureAccess: Bool
    let microphonePermission: String
    let microphoneCaptureSupported: Bool
    let displays: [DisplayInfo]
}

struct RecordOutput: Codable {
    let object: String
    let status: String
    let systemAudioPath: String
    let microphoneAudioPath: String?
    let capturedMicrophone: Bool
    let displayID: UInt32
}

enum HelperError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        switch self {
        case .message(let message):
            return message
        }
    }
}

final class AudioFileWriter {
    let url: URL
    private let lock = NSLock()
    private var audioFile: AVAudioFile?

    init(url: URL) {
        self.url = url
    }

    func append(sampleBuffer: CMSampleBuffer) throws {
        lock.lock()
        defer { lock.unlock() }

        guard CMSampleBufferIsValid(sampleBuffer) else {
            return
        }

        guard let formatDescription = CMSampleBufferGetFormatDescription(sampleBuffer),
              let streamDescription = CMAudioFormatDescriptionGetStreamBasicDescription(formatDescription)
        else {
            throw HelperError.message("sample buffer missing audio format description")
        }

        guard let format = AVAudioFormat(streamDescription: streamDescription) else {
            throw HelperError.message("failed to derive AVAudioFormat from sample buffer")
        }

        let sampleCount = CMSampleBufferGetNumSamples(sampleBuffer)
        guard sampleCount > 0 else {
            return
        }

        guard let pcmBuffer = AVAudioPCMBuffer(
            pcmFormat: format,
            frameCapacity: AVAudioFrameCount(sampleCount)
        ) else {
            throw HelperError.message("failed to allocate PCM buffer")
        }

        pcmBuffer.frameLength = pcmBuffer.frameCapacity

        let status = CMSampleBufferCopyPCMDataIntoAudioBufferList(
            sampleBuffer,
            at: 0,
            frameCount: Int32(sampleCount),
            into: pcmBuffer.mutableAudioBufferList
        )
        guard status == noErr else {
            throw HelperError.message("failed to copy PCM data from sample buffer: \(status)")
        }

        if audioFile == nil {
            audioFile = try AVAudioFile(
                forWriting: url,
                settings: format.settings,
                commonFormat: format.commonFormat,
                interleaved: format.isInterleaved
            )
        }

        try audioFile?.write(from: pcmBuffer)
    }
}

final class Recorder: NSObject, SCStreamOutput {
    private let systemWriter: AudioFileWriter
    private let microphoneWriter: AudioFileWriter?
    private var stream: SCStream?
    private let queue = DispatchQueue(label: "com.scribecli.capture")
    private let waitSemaphore = DispatchSemaphore(value: 0)
    private var stopSources: [DispatchSourceSignal] = []

    init(systemAudioURL: URL, microphoneAudioURL: URL?) {
        self.systemWriter = AudioFileWriter(url: systemAudioURL)
        self.microphoneWriter = microphoneAudioURL.map(AudioFileWriter.init(url:))
    }

    func start(
        displayID: UInt32?,
        captureMicrophone: Bool,
        durationSeconds: UInt64?
    ) async throws -> RecordOutput {
        if !CGPreflightScreenCaptureAccess() {
            let granted = CGRequestScreenCaptureAccess()
            guard granted else {
                throw HelperError.message("screen recording permission was not granted")
            }
        }

        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let display = selectDisplay(from: content.displays, requestedID: displayID) else {
            throw HelperError.message("no shareable display is available for capture")
        }

        let filter = SCContentFilter(display: display, excludingApplications: [], exceptingWindows: [])
        let configuration = SCStreamConfiguration()
        configuration.capturesAudio = true
        configuration.width = 2
        configuration.height = 2
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 2)
        configuration.queueDepth = 3
        configuration.sampleRate = 48_000
        configuration.channelCount = 2
        configuration.excludesCurrentProcessAudio = false

        if captureMicrophone {
            if #available(macOS 15.0, *) {
                configuration.captureMicrophone = true
            } else {
                throw HelperError.message("microphone capture via ScreenCaptureKit requires macOS 15.0 or newer")
            }
        }

        let stream = SCStream(filter: filter, configuration: configuration, delegate: nil)
        try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue)
        if captureMicrophone {
            if #available(macOS 15.0, *) {
                try stream.addStreamOutput(self, type: .microphone, sampleHandlerQueue: queue)
            }
        }
        self.stream = stream

        installStopHandlers()

        try await stream.startCapture()

        if let durationSeconds {
            DispatchQueue.global(qos: .utility).asyncAfter(
                deadline: .now() + .seconds(Int(durationSeconds))
            ) { [waitSemaphore] in
                waitSemaphore.signal()
            }
        }

        waitSemaphore.wait()
        try await stream.stopCapture()
        uninstallStopHandlers()

        return RecordOutput(
            object: "native_capture",
            status: "completed",
            systemAudioPath: systemWriter.url.path,
            microphoneAudioPath: microphoneWriter?.url.path,
            capturedMicrophone: captureMicrophone,
            displayID: display.displayID
        )
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of outputType: SCStreamOutputType) {
        do {
            switch outputType {
            case .audio:
                try systemWriter.append(sampleBuffer: sampleBuffer)
            case .microphone:
                try microphoneWriter?.append(sampleBuffer: sampleBuffer)
            default:
                break
            }
        } catch {
            fputs("capture write error: \(error.localizedDescription)\n", stderr)
        }
    }

    private func installStopHandlers() {
        signal(SIGINT, SIG_IGN)
        signal(SIGTERM, SIG_IGN)

        let intSource = DispatchSource.makeSignalSource(signal: SIGINT, queue: .main)
        intSource.setEventHandler { [weak self] in
            self?.waitSemaphore.signal()
        }
        intSource.resume()

        let termSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        termSource.setEventHandler { [weak self] in
            self?.waitSemaphore.signal()
        }
        termSource.resume()

        stopSources = [intSource, termSource]
    }

    private func uninstallStopHandlers() {
        for source in stopSources {
            source.cancel()
        }
        stopSources.removeAll()
    }
}

private func runDoctor() async throws -> DoctorOutput {
    let access = CGPreflightScreenCaptureAccess()
    let displays: [DisplayInfo]
    if access {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        displays = content.displays.map { display in
            DisplayInfo(
                displayID: display.displayID,
                width: Int(display.width),
                height: Int(display.height),
                isMain: display.displayID == CGMainDisplayID()
            )
        }
    } else {
        displays = []
    }

    let microphonePermission = microphonePermissionStatus()

    return DoctorOutput(
        object: "native_macos_status",
        screenCaptureAccess: access,
        microphonePermission: microphonePermission,
        microphoneCaptureSupported: ProcessInfo.processInfo.isOperatingSystemAtLeast(
            OperatingSystemVersion(majorVersion: 15, minorVersion: 0, patchVersion: 0)
        ),
        displays: displays
    )
}

private func selectDisplay(from displays: [SCDisplay], requestedID: UInt32?) -> SCDisplay? {
    if let requestedID {
        return displays.first { $0.displayID == requestedID }
    }

    if let main = displays.first(where: { $0.displayID == CGMainDisplayID() }) {
        return main
    }

    return displays.first
}

private func printJSON<T: Encodable>(_ value: T) throws {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    let data = try encoder.encode(value)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write("\n".data(using: .utf8)!)
}

private func jsonString(_ value: String) -> String {
    let data = try? JSONEncoder().encode(value)
    return String(data: data ?? Data("\"unknown error\"".utf8), encoding: .utf8) ?? "\"unknown error\""
}

private func microphonePermissionStatus() -> String {
    switch AVCaptureDevice.authorizationStatus(for: .audio) {
    case .authorized:
        return "authorized"
    case .denied:
        return "denied"
    case .restricted:
        return "restricted"
    case .notDetermined:
        return "not_determined"
    @unknown default:
        return "unknown"
    }
}

private struct RecordOptions {
    let systemAudioPath: String
    let microphoneAudioPath: String?
    let captureMicrophone: Bool
    let durationSeconds: UInt64?
    let displayID: UInt32?
}

private enum Command {
    case doctor
    case listDisplays
    case record(RecordOptions)
}

private struct ArgumentParser {
    let command: Command

    init(arguments: [String]) throws {
        guard let first = arguments.first else {
            throw HelperError.message("missing command")
        }

        switch first {
        case "doctor":
            command = .doctor
        case "list-displays":
            command = .listDisplays
        case "record":
            command = .record(try Self.parseRecord(arguments: Array(arguments.dropFirst())))
        default:
            throw HelperError.message("unknown command: \(first)")
        }
    }

    private static func parseRecord(arguments: [String]) throws -> RecordOptions {
        var systemAudioPath: String?
        var microphoneAudioPath: String?
        var captureMicrophone = false
        var durationSeconds: UInt64?
        var displayID: UInt32?

        var index = 0
        while index < arguments.count {
            let argument = arguments[index]
            switch argument {
            case "--system-audio-path":
                systemAudioPath = try requireValue(after: index, in: arguments, flag: argument)
                index += 2
            case "--microphone-audio-path":
                microphoneAudioPath = try requireValue(after: index, in: arguments, flag: argument)
                index += 2
            case "--capture-microphone":
                let raw = try requireValue(after: index, in: arguments, flag: argument)
                captureMicrophone = raw == "true"
                index += 2
            case "--duration-seconds":
                let raw = try requireValue(after: index, in: arguments, flag: argument)
                durationSeconds = UInt64(raw)
                index += 2
            case "--display-id":
                let raw = try requireValue(after: index, in: arguments, flag: argument)
                displayID = UInt32(raw)
                index += 2
            default:
                throw HelperError.message("unknown record flag: \(argument)")
            }
        }

        guard let systemAudioPath else {
            throw HelperError.message("--system-audio-path is required")
        }

        if captureMicrophone && microphoneAudioPath == nil {
            throw HelperError.message("--microphone-audio-path is required when microphone capture is enabled")
        }

        return RecordOptions(
            systemAudioPath: systemAudioPath,
            microphoneAudioPath: microphoneAudioPath,
            captureMicrophone: captureMicrophone,
            durationSeconds: durationSeconds,
            displayID: displayID
        )
    }

    private static func requireValue(after index: Int, in arguments: [String], flag: String) throws -> String {
        let valueIndex = index + 1
        guard valueIndex < arguments.count else {
            throw HelperError.message("missing value for \(flag)")
        }
        return arguments[valueIndex]
    }
}

Task {
    do {
        let parser = try ArgumentParser(arguments: Array(CommandLine.arguments.dropFirst()))
        switch parser.command {
        case .doctor:
            let output = try await runDoctor()
            try printJSON(output)
        case .listDisplays:
            let output = try await runDoctor()
            try printJSON(output)
        case .record(let options):
            let recorder = Recorder(
                systemAudioURL: URL(fileURLWithPath: options.systemAudioPath),
                microphoneAudioURL: options.microphoneAudioPath.map { URL(fileURLWithPath: $0) }
            )
            let output = try await recorder.start(
                displayID: options.displayID,
                captureMicrophone: options.captureMicrophone,
                durationSeconds: options.durationSeconds
            )
            try printJSON(output)
        }
        exit(0)
    } catch {
        let message = error.localizedDescription
        fputs("{\"object\":\"error\",\"message\":\(jsonString(message))}\n", stderr)
        exit(1)
    }
}

dispatchMain()
