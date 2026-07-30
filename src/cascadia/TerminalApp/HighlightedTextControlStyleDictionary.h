// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#pragma once

#include "HighlightedTextControlStyleDictionary.g.h"

namespace winrt::TerminalApp::implementation
{
    struct HighlightedTextControlStyleDictionary : HighlightedTextControlStyleDictionaryT<HighlightedTextControlStyleDictionary>
    {
        HighlightedTextControlStyleDictionary();
    };
}

namespace winrt::TerminalApp::factory_implementation
{
    BASIC_FACTORY(HighlightedTextControlStyleDictionary);
}
