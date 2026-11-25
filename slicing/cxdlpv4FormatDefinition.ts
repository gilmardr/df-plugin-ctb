import type { SlicingFormatDefinition } from '@/features/slicing/formats/types';

/**
 * Creality CXDLPv4, read by the HALOT-MAGE family. Its layers use the CTB RLE and
 * its print parameters map onto the CTB resin settings, so the CTB plugin encodes
 * it (see slicing/rust/cxdlpv4_encoder.rs). The HALOT-X1 pixel-pair codec is not
 * implemented, and X1 jobs are refused by the encoder.
 */
export const CXDLPV4_FORMAT_DEFINITION: SlicingFormatDefinition = {
  id: 'ctb.cxdlpv4.v1',
  outputFormat: '.cxdlpv4',
  displayName: 'CXDLPv4',
  ownership: 'plugin',
  layerDataKind: 'raw-mask',
  pluginId: 'ctb',
  settingsModes: [
    { value: 'simple', label: 'Simple', isDefault: true },
    { value: 'twostage', label: 'Two Stage' },
  ],
  rustModulePath: 'formats::cxdlpv4',
  wasmExportName: 'encode_cxdlpv4_container',
  notes: 'Creality CXDLPv4 encoder (HALOT-MAGE family) sharing the CTB layer RLE and previews.',
};
