#pragma once

struct EyeInput {
    float orientation[4]; // x, y, z, w
    float position[3];
    float fovLeft;
    float fovRight;
    float fovTop;
    float fovBottom;
};

struct OnCreateResult {
    int streamSurfaceHandle;
    int loadingSurfaceHandle;
};

struct StreamConfigInput {
    unsigned int viewWidth;
    unsigned int viewHeight;
    bool enableFoveation;
    float foveationCenterSizeX;
    float foveationCenterSizeY;
    float foveationCenterShiftX;
    float foveationCenterShiftY;
    float foveationEdgeRatioX;
    float foveationEdgeRatioY;
};

// gltf_model.h
extern "C" const unsigned char *LOBBY_ROOM_GLTF_PTR;
extern "C" unsigned int LOBBY_ROOM_GLTF_LEN;
extern "C" const unsigned char *LOBBY_ROOM_BIN_PTR;
extern "C" unsigned int LOBBY_ROOM_BIN_LEN;

// graphics.h
extern "C" void initGraphicsNative();
extern "C" void destroyGraphicsNative();
extern "C" void prepareLobbyRoom(int viewWidth,
                                 int viewHeight,
                                 const int *swapchainTextures[2],
                                 int swapchainLength);
extern "C" void destroyRenderers();
extern "C" void setStreamConfig(StreamConfigInput config);
extern "C" void streamStartNative(const int *swapchainTextures[2], int swapchainLength);
extern "C" void updateLobbyHudTexture(const unsigned char *data);
extern "C" void renderLobbyNative(const EyeInput eyeInputs[2], const int swapchainIndices[2]);
extern "C" void renderStreamNative(void *streamHardwareBuffer, const int swapchainIndices[2]);
