// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

#pragma once

#include "CommonResourcesDictionary.g.h"
#include "SettingContainerStyleDictionary.g.h"

namespace winrt::Microsoft::Terminal::Settings::Editor::implementation
{
    struct CommonResourcesDictionary : CommonResourcesDictionaryT<CommonResourcesDictionary>
    {
        CommonResourcesDictionary();
    };

    struct SettingContainerStyleDictionary : SettingContainerStyleDictionaryT<SettingContainerStyleDictionary>
    {
        SettingContainerStyleDictionary();
    };
}

namespace winrt::Microsoft::Terminal::Settings::Editor::factory_implementation
{
    BASIC_FACTORY(CommonResourcesDictionary);
    BASIC_FACTORY(SettingContainerStyleDictionary);
}
