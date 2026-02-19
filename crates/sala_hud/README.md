# sala_hud

Holographic HUD panel for TalaSala IDE - a GPUI-based right panel that connects to the tais-devcontainerd daemon via gRPC.

## Purpose

This crate implements the "Proposal-First UX" for DevContainer scaffold generation (Flow 0 from the Sonika IDE specs). It provides:

- A right-side panel in the Sala editor (Zed fork)
- gRPC client connection to the tais-devcontainerd daemon
- Real-time scaffold proposal display
- User confirmation workflow for file generation

## Current Status

**Phase 1 - Minimal Integration (Current)**
- Panel registration and action handling
- gRPC client stub (connection not implemented)
- Placeholder UI rendering

## Architecture

```
┌─────────────────┐         gRPC          ┌──────────────────────┐
│   Sala Editor   │ ◄──────────────────► │  tais-devcontainerd  │
│   (sala_hud)    │   Unix socket/Pipe    │  (DevContainer       │
│                 │                       │   Service)           │
└─────────────────┘                       └──────────────────────┘
```

## Usage

The HUD can be toggled via:
- Command Palette: "sala_hud: Toggle Hud"
- Keyboard shortcut (if configured)

## Dependencies

- **gpui**: UI framework
- **workspace**: Zed panel integration
- **ui/theme**: Styling

## Future Work

- Implement gRPC connection to tais-devcontainerd daemon (tonic + prost)
- Build scaffold proposal UI components
- Add real-time intent parsing feedback
- Implement confirmation workflow
