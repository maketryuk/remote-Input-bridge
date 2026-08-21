// Generates mac-receiver/Resources/AppIcon.icns.
//
// Drawn in code rather than shipped as a binary asset so the icon stays reviewable in the repo
// and needs no design tooling to change.
import AppKit

let sizes = [16, 32, 64, 128, 256, 512, 1024]
// 64 exists only to become icon_32x32@2x; 1024 only to become icon_512x512@2x.
let iconset = URL(fileURLWithPath: CommandLine.arguments[1])
try? FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

func draw(size: Int) -> Data {
    let side = CGFloat(size)
    let image = NSImage(size: NSSize(width: side, height: side))
    image.lockFocus()
    guard let context = NSGraphicsContext.current?.cgContext else { image.unlockFocus(); return Data() }
    context.setShouldAntialias(true)

    // Squircle background with a vertical gradient.
    let inset = side * 0.06
    let rect = CGRect(x: inset, y: inset, width: side - inset * 2, height: side - inset * 2)
    let path = CGPath(
        roundedRect: rect,
        cornerWidth: rect.width * 0.225,
        cornerHeight: rect.height * 0.225,
        transform: nil
    )
    context.saveGState()
    context.addPath(path)
    context.clip()
    let colors = [
        CGColor(red: 0.22, green: 0.28, blue: 0.85, alpha: 1),
        CGColor(red: 0.44, green: 0.20, blue: 0.72, alpha: 1),
    ]
    if let gradient = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: colors as CFArray,
        locations: [0, 1]
    ) {
        context.drawLinearGradient(
            gradient,
            start: CGPoint(x: 0, y: side),
            end: CGPoint(x: side, y: 0),
            options: []
        )
    }
    context.restoreGState()

    // Two signal arcs on the left: input arriving from the machine on the right.
    context.setStrokeColor(CGColor(red: 1, green: 1, blue: 1, alpha: 0.55))
    context.setLineCap(.round)
    for (index, radius) in [0.26, 0.36].enumerated() {
        context.setLineWidth(side * (index == 0 ? 0.045 : 0.036))
        let box = CGRect(
            x: side * (0.5 - radius), y: side * (0.5 - radius),
            width: side * radius * 2, height: side * radius * 2
        )
        context.addArc(
            center: CGPoint(x: box.midX, y: box.midY),
            radius: box.width / 2,
            startAngle: .pi * 0.72,
            endAngle: .pi * 1.28,
            clockwise: false
        )
        context.strokePath()
    }

    // Pointer, drawn as the classic arrow so the icon reads at 16 px.
    let unit = side / 100
    context.translateBy(x: side * 0.40, y: side * 0.76)
    context.scaleBy(x: unit, y: -unit)
    let arrow = CGMutablePath()
    arrow.move(to: CGPoint(x: 0, y: 0))
    arrow.addLine(to: CGPoint(x: 0, y: 44))
    arrow.addLine(to: CGPoint(x: 11.5, y: 33))
    arrow.addLine(to: CGPoint(x: 18.5, y: 49))
    arrow.addLine(to: CGPoint(x: 27, y: 45))
    arrow.addLine(to: CGPoint(x: 20, y: 29.5))
    arrow.addLine(to: CGPoint(x: 35, y: 27))
    arrow.closeSubpath()
    context.setShadow(offset: CGSize(width: 0, height: -1.5), blur: 3)
    context.setFillColor(CGColor(red: 1, green: 1, blue: 1, alpha: 1))
    context.addPath(arrow)
    context.fillPath()

    image.unlockFocus()

    guard let tiff = image.tiffRepresentation,
          let rep = NSBitmapImageRep(data: tiff),
          let png = rep.representation(using: .png, properties: [:])
    else { return Data() }
    return png
}

// iconutil only accepts this exact set of names; anything else makes it fail.
for size in sizes {
    let png = draw(size: size)
    if size <= 512 {
        try? png.write(to: iconset.appendingPathComponent("icon_\(size)x\(size).png"))
    }
    let half = size / 2
    if half >= 16 {
        try? png.write(to: iconset.appendingPathComponent("icon_\(half)x\(half)@2x.png"))
    }
}
print("iconset written to \(iconset.path)")
